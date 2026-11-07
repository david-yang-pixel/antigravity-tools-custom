//! Per-connection conversation record. Upstream response state belongs to the account (and
//! connection) that produced it, so a turn continuing a `previous_response_id` elsewhere is
//! rebuilt from this record as one full request.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

/// Bytes of conversation kept per connection; the oldest items go first above it.
pub const TRANSCRIPT_LIMIT: usize = 32 * 1024 * 1024;
/// Rejected ciphertext digests remembered per connection.
const MAX_REJECTED: usize = 512;

#[derive(Debug, PartialEq)]
pub enum Rebuild {
    /// The whole conversation up to the referenced response plus the new input, without
    /// encrypted reasoning or compaction items.
    Full(Vec<Value>),
    /// The referenced response was never recorded on this connection.
    Unknown,
    /// A tool output would be sent without its call.
    Orphaned,
    /// Part of the referenced history was dropped by the memory bound.
    Dropped,
}

type Digest32 = [u8; 32];

pub struct Transcript {
    limit: usize,
    /// The current conversation with each item's serialized size.
    items: VecDeque<(Value, usize)>,
    bytes: usize,
    /// Items dropped from the front of the current conversation.
    dropped: usize,
    /// Response id → absolute end position (dropped + index) of its output.
    responses: HashMap<String, usize>,
    /// The current turn continues a response this record never saw; nothing is recorded
    /// until the next turn starts from known state.
    detached: bool,
    rejected: VecDeque<Digest32>,
    rejected_set: HashSet<Digest32>,
}

fn digest(item: &Value) -> Option<Digest32> {
    let cipher = item.get("encrypted_content")?.as_str()?;
    Some(Sha256::digest(cipher.as_bytes()).into())
}

fn kind(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Hidden reasoning and compaction state minted by one account cannot be decrypted by another.
pub(super) fn encrypted_state(item: &Value) -> bool {
    matches!(
        kind(item),
        "reasoning" | "compaction" | "compaction_summary"
    )
}

/// Whether some tool output answers no call earlier in `items`.
fn orphaned(items: &[Value]) -> bool {
    let mut calls = HashSet::new();
    for item in items {
        let kind = kind(item);
        let call_id = item.get("call_id").and_then(Value::as_str);
        if kind.ends_with("_call_output") {
            if !call_id.is_some_and(|id| calls.contains(id)) {
                return true;
            }
        } else if kind.ends_with("_call") {
            if let Some(id) = call_id {
                calls.insert(id.to_owned());
            }
        }
    }
    false
}

impl Transcript {
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            limit: limit_bytes,
            items: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            responses: HashMap::new(),
            detached: false,
            rejected: VecDeque::new(),
            rejected_set: HashSet::new(),
        }
    }

    fn end(&self) -> usize {
        self.dropped + self.items.len()
    }

    /// Positions the record for a new turn: a turn without `previous_response_id` starts a
    /// new conversation; one continuing a recorded response keeps the history up to it
    /// (dropping any branch after it); one continuing an unknown response is not recorded.
    pub fn begin(&mut self, previous_response_id: Option<&str>) {
        let position = previous_response_id.map(|id| self.responses.get(id).copied());
        match position {
            Some(Some(position)) => {
                if position >= self.dropped {
                    while self.end() > position {
                        let (_, size) = self.items.pop_back().expect("end > dropped");
                        self.bytes -= size;
                    }
                } else {
                    self.items.clear();
                    self.bytes = 0;
                    self.dropped = position;
                }
                self.responses.retain(|_, end| *end <= position);
                self.detached = false;
            }
            unknown => {
                self.items.clear();
                self.bytes = 0;
                self.dropped = 0;
                self.responses.clear();
                self.detached = unknown.is_some();
            }
        }
    }

    fn push(&mut self, items: &[Value]) {
        for item in items {
            if digest(item).is_some_and(|d| self.rejected_set.contains(&d)) {
                continue;
            }
            let size = serde_json::to_vec(item).map_or(0, |b| b.len());
            self.items.push_back((item.clone(), size));
            self.bytes += size;
        }
        while self.bytes > self.limit {
            let Some((_, size)) = self.items.pop_front() else {
                break;
            };
            self.bytes -= size;
            self.dropped += 1;
        }
    }

    /// The input a turn sent (its new items when it continues a response).
    pub fn record_request(&mut self, input: &[Value]) {
        if !self.detached {
            self.push(input);
        }
    }

    /// A completed turn's output, addressable by its upstream response id.
    pub fn record_response(&mut self, response_id: &str, output: &[Value]) {
        if self.detached {
            return;
        }
        self.push(output);
        self.responses.insert(response_id.to_owned(), self.end());
    }

    /// A full request continuing `previous_response_id` with `new_input`.
    pub fn rebuild(&self, previous_response_id: &str, new_input: &[Value]) -> Rebuild {
        let Some(&position) = self.responses.get(previous_response_id) else {
            return Rebuild::Unknown;
        };
        if self.dropped > 0 && position > 0 {
            return Rebuild::Dropped;
        }
        let items: Vec<Value> = self
            .items
            .iter()
            .take(position)
            .map(|(item, _)| item)
            .chain(new_input)
            .filter(|item| !encrypted_state(item))
            .filter(|item| !digest(item).is_some_and(|d| self.rejected_set.contains(&d)))
            .cloned()
            .collect();
        if orphaned(&items) {
            return Rebuild::Orphaned;
        }
        Rebuild::Full(items)
    }

    /// Remembers the ciphertexts of `items` as rejected by the upstream and removes them
    /// from the record.
    pub fn reject(&mut self, items: &[Value]) {
        for d in items.iter().filter_map(digest) {
            if !self.rejected_set.insert(d) {
                continue;
            }
            self.rejected.push_back(d);
            if self.rejected.len() > MAX_REJECTED {
                if let Some(old) = self.rejected.pop_front() {
                    self.rejected_set.remove(&old);
                }
            }
        }
        let mut removed = Vec::new();
        let mut kept = VecDeque::with_capacity(self.items.len());
        for (index, (item, size)) in std::mem::take(&mut self.items).into_iter().enumerate() {
            if digest(&item).is_some_and(|d| self.rejected_set.contains(&d)) {
                removed.push(self.dropped + index);
                self.bytes -= size;
            } else {
                kept.push_back((item, size));
            }
        }
        self.items = kept;
        if !removed.is_empty() {
            for end in self.responses.values_mut() {
                *end -= removed.iter().filter(|p| **p < *end).count();
            }
        }
    }

    /// Removes items whose ciphertext the upstream rejected earlier on this connection.
    pub fn strip_rejected(&self, items: &mut Vec<Value>) {
        items.retain(|item| !digest(item).is_some_and(|d| self.rejected_set.contains(&d)));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}
