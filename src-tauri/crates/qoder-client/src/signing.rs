//! Host the exact, checksum-pinned 1.1.83 wasm-bindgen module without a JS runtime.
//! Only strings, maps, byte arrays, wall time and OS randomness cross the boundary.
use crate::Error;
use rand::RngCore;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use wasmi::{
    Caller, Config, Engine, Extern, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, Val,
};

const MODULE_BYTES: &[u8] = include_bytes!("../vendor/qoder_auth.wasm");
/// Maximum size of any string or byte buffer crossing the WASM boundary.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
const FUEL: u64 = 200_000_000;

#[derive(Clone, Default)]
enum Object {
    #[default]
    Undefined,
    Global,
    Crypto,
    String(String),
    Map(Arc<Mutex<BTreeMap<String, String>>>),
    Bytes(Arc<Mutex<Vec<u8>>>),
    View(usize, usize),
}
struct Host {
    heap: Vec<Object>,
    free: Vec<usize>,
    limits: StoreLimits,
    #[cfg(test)]
    fixture_counter: Option<u64>,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            // wasm-bindgen reserves 0..1027 (undefined, null, true, false included).
            heap: vec![Object::Undefined; 1028],
            free: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(64 * 1024 * 1024)
                .instances(1)
                .memories(1)
                .build(),
            #[cfg(test)]
            fixture_counter: None,
        }
    }
}
impl Host {
    fn add(&mut self, value: Object) -> Result<i32, wasmi::Error> {
        let id = if let Some(id) = self.free.pop() {
            self.heap[id] = value;
            id
        } else {
            if self.heap.len() >= 16384 {
                return Err(trap());
            }
            self.heap.push(value);
            self.heap.len() - 1
        };
        Ok(id as i32)
    }
    fn get(&self, id: i32) -> Result<Object, wasmi::Error> {
        self.heap.get(id as usize).cloned().ok_or_else(trap)
    }
    fn take(&mut self, id: i32) -> Result<Object, wasmi::Error> {
        let value = self.get(id)?;
        if id >= 1028 {
            self.heap[id as usize] = Object::Undefined;
            self.free.push(id as usize);
        }
        Ok(value)
    }
}
fn trap() -> wasmi::Error {
    wasmi::Error::new("Invalid Qoder WASM host operation")
}
fn memory(c: &Caller<'_, Host>) -> Result<Memory, wasmi::Error> {
    c.get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(trap)
}
fn read(c: &Caller<'_, Host>, ptr: usize, len: usize) -> Result<Vec<u8>, wasmi::Error> {
    if len > MAX_BYTES {
        return Err(trap());
    }
    memory(c)?
        .data(c)
        .get(ptr..ptr.checked_add(len).ok_or_else(trap)?)
        .map(|b| b.to_vec())
        .ok_or_else(trap)
}
fn bytes(c: &Caller<'_, Host>, value: Object) -> Result<Vec<u8>, wasmi::Error> {
    match value {
        Object::Bytes(v) => Ok(v.lock().map_err(|_| trap())?.clone()),
        Object::View(p, n) => read(c, p, n),
        _ => Err(trap()),
    }
}
fn string(value: Object) -> Result<String, wasmi::Error> {
    if let Object::String(value) = value {
        Ok(value)
    } else {
        Err(trap())
    }
}
fn random(c: &mut Caller<'_, Host>, value: Object) -> Result<(), wasmi::Error> {
    fn fill(c: &mut Caller<'_, Host>, bytes: &mut [u8]) -> Result<(), wasmi::Error> {
        #[cfg(test)]
        if let Some(counter) = c.data_mut().fixture_counter.as_mut() {
            for byte in bytes {
                *counter += 1;
                *byte = ((*counter % 255) + 1) as u8;
            }
            return Ok(());
        }
        let _ = c;
        rand::rngs::OsRng.try_fill_bytes(bytes).map_err(|_| trap())
    }
    match value {
        Object::Bytes(v) => fill(c, &mut v.lock().map_err(|_| trap())?),
        Object::View(p, n) if n <= MAX_BYTES => {
            let mut data = vec![0; n];
            fill(c, &mut data)?;
            memory(c)?.write(c, p, &data).map_err(|_| trap())
        }
        _ => Err(trap()),
    }
}
fn host_call(
    name: &str,
    mut c: Caller<'_, Host>,
    args: &[Val],
    out: &mut [Val],
) -> Result<(), wasmi::Error> {
    let arg = |n: usize| args.get(n).and_then(Val::i32).ok_or_else(trap);
    let mut value = None;
    match name {
        "__wbindgen_object_drop_ref" => {
            c.data_mut().take(arg(0)?)?;
        }
        "__wbindgen_object_clone_ref" => value = Some(c.data().get(arg(0)?)?),
        "__wbg_new_99cabae501c0a8a0" => value = Some(Object::Map(Default::default())),
        "__wbg_set_08463b1df38a7e29" => {
            let map = c.data().get(arg(0)?)?;
            let key = string(c.data().get(arg(1)?)?)?;
            let val = string(c.data().get(arg(2)?)?)?;
            if let Object::Map(ref map) = map {
                map.lock().map_err(|_| trap())?.insert(key, val);
            } else {
                return Err(trap());
            }
            value = Some(map);
        }
        "__wbg_getRandomValues_d49329ff89a07af1" => {
            random(&mut c, Object::View(arg(0)? as usize, arg(1)? as usize))?
        }
        "__wbg_getRandomValues_c44a50d8cfdaebeb" | "__wbg_randomFillSync_6c25eac9869eb53c" => {
            let data = c.data().get(arg(1)?)?;
            random(&mut c, data)?;
        }
        "__wbg_crypto_38df2bab126b63dc" => value = Some(Object::Crypto),
        "__wbg_static_accessor_GLOBAL_THIS_a1248013d790bf5f"
        | "__wbg_static_accessor_GLOBAL_f2e0f995a21329ff" => value = Some(Object::Global),
        "__wbg_static_accessor_SELF_24f78b6d23f286ea"
        | "__wbg_static_accessor_WINDOW_59fd959c540fe405" => out[0] = Val::I32(0),
        // Expose a WebCrypto host, not a Node environment/module loader.
        "__wbg_process_44c7a14e11e9f69e"
        | "__wbg_versions_276b2795b1c6a219"
        | "__wbg_node_84ea875411254db1"
        | "__wbg_msCrypto_bd5a034af96bcba6"
        | "__wbg_require_b4edbdcf3e2a1ef0" => value = Some(Object::Undefined),
        "__wbg_new_with_length_9cedd08484b73942" => {
            let n = arg(0)? as usize;
            if n > MAX_BYTES {
                return Err(trap());
            }
            value = Some(Object::Bytes(Arc::new(Mutex::new(vec![0; n]))));
        }
        "__wbg_subarray_0f98d3fb634508ad" => {
            let data = c.data().get(arg(0)?)?;
            let start = arg(1)? as usize;
            let end = arg(2)? as usize;
            value = Some(match data {
                Object::View(p, n) if start <= end && end <= n => {
                    Object::View(p + start, end - start)
                }
                data => Object::Bytes(Arc::new(Mutex::new(
                    bytes(&c, data)?.get(start..end).ok_or_else(trap)?.to_vec(),
                ))),
            });
        }
        "__wbg_length_0c32cb8543c8e4c8" => {
            out[0] = Val::I32(bytes(&c, c.data().get(arg(0)?)?)?.len() as i32)
        }
        "__wbg_prototypesetcall_3e05eb9545565046" => {
            let data = bytes(&c, c.data().get(arg(2)?)?)?;
            if data.len() > arg(1)? as usize {
                return Err(trap());
            }
            memory(&c)?
                .write(&mut c, arg(0)? as usize, &data)
                .map_err(|_| trap())?;
        }
        "__wbindgen_cast_0000000000000001" => {
            value = Some(Object::View(arg(0)? as usize, arg(1)? as usize))
        }
        "__wbindgen_cast_0000000000000002" | "__wbg_Error_2e59b1b37a9a34c3" => {
            value = Some(Object::String(
                String::from_utf8(read(&c, arg(0)? as usize, arg(1)? as usize)?)
                    .map_err(|_| trap())?,
            ))
        }
        "__wbg_now_88621c9c9a4f3ffc" => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| trap())?
                .as_millis() as f64;
            #[cfg(test)]
            let now = if c.data().fixture_counter.is_some() {
                1_800_000_000_000.0
            } else {
                now
            };
            out[0] = Val::F64(now.into());
        }
        "__wbg___wbindgen_is_undefined_c0cca72b82b86f4d" => {
            out[0] = Val::I32(matches!(c.data().get(arg(0)?)?, Object::Undefined) as i32)
        }
        "__wbg___wbindgen_is_object_40c5a80572e8f9d3" => {
            out[0] = Val::I32(matches!(
                c.data().get(arg(0)?)?,
                Object::Global
                    | Object::Crypto
                    | Object::Map(_)
                    | Object::Bytes(_)
                    | Object::View(..)
            ) as i32)
        }
        "__wbg___wbindgen_is_string_b29b5c5a8065ba1a" => {
            out[0] = Val::I32(matches!(c.data().get(arg(0)?)?, Object::String(_)) as i32)
        }
        "__wbg___wbindgen_is_function_49868bde5eb1e745" => out[0] = Val::I32(0),
        "__wbg___wbindgen_throw_81fc77679af83bc6" | "__wbg_call_d578befcc3145dee" => {
            return Err(trap())
        }
        _ => return Err(trap()),
    }
    if let Some(value) = value {
        out[0] = Val::I32(c.data_mut().add(value)?);
    }
    Ok(())
}

struct Vm {
    store: Store<Host>,
    instance: Instance,
}
impl Vm {
    fn new() -> Result<Self, Error> {
        static MODULE: OnceLock<Result<(Engine, Module), String>> = OnceLock::new();
        let (engine, module) = MODULE
            .get_or_init(|| {
                let mut config = Config::default();
                config.consume_fuel(true);
                let engine = Engine::new(&config);
                Module::new(&engine, MODULE_BYTES)
                    .map(|module| (engine, module))
                    .map_err(|_| "Cannot load Qoder signer".into())
            })
            .as_ref()
            .map_err(|e| e.clone())?;
        let mut store = Store::new(engine, Host::default());
        store.limiter(|h| &mut h.limits);
        store.set_fuel(FUEL)?;
        let mut linker = Linker::new(engine);
        for import in module.imports() {
            let name = import.name().to_owned();
            let ty = import
                .ty()
                .func()
                .ok_or("Unexpected Qoder WASM import")?
                .clone();
            linker.func_new(import.module(), import.name(), ty, move |c, a, o| {
                host_call(&name, c, a, o)
            })?;
        }
        let instance = linker.instantiate_and_start(&mut store, module)?;
        Ok(Self { store, instance })
    }
    fn call(&mut self, name: &str, args: &[i32]) -> Result<Vec<Val>, Error> {
        let f = self
            .instance
            .get_func(&self.store, name)
            .ok_or("Missing Qoder WASM export")?;
        let mut out: Vec<_> = f
            .ty(&self.store)
            .results()
            .iter()
            .map(|t| Val::default(*t))
            .collect();
        let args: Vec<_> = args.iter().copied().map(Val::I32).collect();
        f.call(&mut self.store, &args, &mut out)
            .map_err(|_| "Qoder WASM operation failed")?;
        Ok(out)
    }
    fn int(&mut self, name: &str, args: &[i32]) -> Result<i32, Error> {
        self.call(name, args)?
            .first()
            .and_then(Val::i32)
            .ok_or_else(|| "Invalid Qoder WASM result".into())
    }
    fn memory(&self) -> Result<Memory, Error> {
        self.instance
            .get_memory(&self.store, "memory")
            .ok_or_else(|| "Missing Qoder WASM memory".into())
    }
    fn put(&mut self, value: Option<&str>) -> Result<[i32; 2], Error> {
        let Some(value) = value else {
            return Ok([0, 0]);
        };
        if value.len() > MAX_BYTES {
            return Err("Qoder signing input exceeds size limit".into());
        }
        let p = self.int("__wbindgen_export2", &[value.len() as i32, 1])?;
        self.memory()?
            .write(&mut self.store, p as usize, value.as_bytes())?;
        Ok([p, value.len() as i32])
    }
    fn words(&self, p: i32) -> Result<[i32; 4], Error> {
        let mut bytes = [0; 16];
        self.memory()?.read(&self.store, p as usize, &mut bytes)?;
        Ok(std::array::from_fn(|i| {
            i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        }))
    }
    fn take_string(&mut self, p: i32, n: i32) -> Result<String, Error> {
        if !(0..=MAX_BYTES as i32).contains(&n) {
            return Err("Qoder WASM result exceeds size limit".into());
        }
        let mut bytes = vec![0; n as usize];
        self.memory()?.read(&self.store, p as usize, &mut bytes)?;
        self.call("__wbindgen_export4", &[p, n, 1])?;
        Ok(String::from_utf8(bytes)?)
    }
    fn frame<T>(&mut self, f: impl FnOnce(&mut Self, i32) -> Result<T, Error>) -> Result<T, Error> {
        self.store.set_fuel(FUEL)?;
        let p = self.int("__wbindgen_add_to_stack_pointer", &[-16])?;
        let result = f(self, p);
        let restored = self.int("__wbindgen_add_to_stack_pointer", &[16]);
        restored?;
        result
    }
    fn string_function(&mut self, name: &str, input: &str) -> Result<String, Error> {
        self.frame(|vm, ret| {
            let [p, n] = vm.put(Some(input))?;
            vm.call(name, &[ret, p, n])?;
            let [p, n, err, failed] = vm.words(ret)?;
            if failed != 0 {
                vm.store.data_mut().take(err)?;
                return Err("Qoder WASM rejected input".into());
            }
            vm.take_string(p, n)
        })
    }
    fn result(&mut self, p: i32) -> Result<PreparedRequest, Error> {
        let result = (|| {
            let url = self.frame(|vm, ret| {
                vm.call("requestresult_url", &[ret, p])?;
                let [ptr, n, ..] = vm.words(ret)?;
                vm.take_string(ptr, n)
            })?;
            let body = self.frame(|vm, ret| {
                vm.call("requestresult_body", &[ret, p])?;
                let [ptr, n, ..] = vm.words(ret)?;
                if ptr == 0 {
                    Ok(None)
                } else {
                    vm.take_string(ptr, n).map(Some)
                }
            })?;
            let h = self.int("requestresult_headers", &[p])?;
            let Object::Map(headers) = self.store.data_mut().take(h)? else {
                return Err("Invalid Qoder WASM headers".into());
            };
            let headers = headers
                .lock()
                .map_err(|_| "Qoder headers unavailable")?
                .clone();
            Ok(PreparedRequest { url, body, headers })
        })();
        self.call("__wbg_requestresult_free", &[p, 0])?;
        result
    }
}

pub struct PreparedRequest {
    pub url: String,
    pub body: Option<String>,
    pub headers: BTreeMap<String, String>,
}
pub struct Signer {
    vm: Vm,
    context: i32,
}
impl Signer {
    pub fn new(machine_id: &str, version: &str, identity: Value) -> Result<Self, Error> {
        Self::with_vm(Vm::new()?, machine_id, version, identity)
    }
    fn with_vm(
        mut vm: Vm,
        machine_id: &str,
        version: &str,
        mut identity: Value,
    ) -> Result<Self, Error> {
        let auth: Value = serde_json::from_str(
            &vm.string_function("generate_runtime_auth_fields", &identity.to_string())?,
        )?;
        identity["encrypt_user_info"] = auth["encrypt_user_info"].clone();
        identity["key"] = auth["key"].clone();
        let context = vm.frame(|vm, ret| {
            let mut args = vec![ret];
            for value in [machine_id, version, &identity.to_string(), &json!({"client_type":"5","business_product":"cli","business_type":"agent","scene":"assistant"}).to_string()] {
                args.extend(vm.put(Some(value))?);
            }
            vm.call("qodercontext_new", &args)?;
            let [p, err, failed, ..] = vm.words(ret)?;
            if failed != 0 { vm.store.data_mut().take(err)?; return Err("Cannot initialize Qoder identity".into()); }
            Ok(p)
        })?;
        Ok(Self { vm, context })
    }
    pub fn inference(
        &mut self,
        endpoint: &str,
        body: &str,
        model: &str,
    ) -> Result<PreparedRequest, Error> {
        let context = self.context;
        let p = self.vm.frame(|vm, ret| {
            let mut args = vec![ret, context];
            for value in [endpoint, body, model, "system"] {
                args.extend(vm.put(Some(value))?);
            }
            vm.call("qodercontext_prepareInferRequest", &args)?;
            let [p, err, failed, ..] = vm.words(ret)?;
            if failed != 0 {
                vm.store.data_mut().take(err)?;
                return Err("Cannot sign Qoder inference".into());
            }
            Ok(p)
        })?;
        self.vm.result(p)
    }
    pub fn request(
        &mut self,
        endpoint: &str,
        path: &str,
        method: &str,
        body: Option<&str>,
    ) -> Result<PreparedRequest, Error> {
        let context = self.context;
        let p = self.vm.frame(|vm, ret| {
            let mut args = vec![ret, context];
            for value in [
                Some(endpoint),
                Some(path),
                Some(method),
                Some("auth"),
                body,
                None,
            ] {
                args.extend(vm.put(value)?);
            }
            vm.call("qodercontext_prepareRequest", &args)?;
            let [p, err, failed, ..] = vm.words(ret)?;
            if failed != 0 {
                vm.store.data_mut().take(err)?;
                return Err("Cannot sign Qoder request".into());
            }
            Ok(p)
        })?;
        self.vm.result(p)
    }
}
impl Drop for Signer {
    fn drop(&mut self) {
        let _ = self.vm.call("__wbg_qodercontext_free", &[self.context, 0]);
    }
}
/// The upstream also returns plaintext JSON. Decryption is applied before JSON/SSE
/// normalization; malformed encrypted data is an error, never silently dropped.
pub fn decrypt(input: &str) -> Result<String, Error> {
    if input.trim_start().starts_with(['{', '[']) || input.trim().is_empty() {
        return Ok(input.to_owned());
    }
    static VM: OnceLock<Mutex<Option<Vm>>> = OnceLock::new();
    let mut slot = VM
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "Qoder decoder unavailable")?;
    if slot.is_none() {
        *slot = Some(Vm::new()?);
    }
    let result = slot
        .as_mut()
        .ok_or("Qoder decoder unavailable")?
        .string_function("decrypt_server_response", input);
    if result.is_err() {
        *slot = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_official_js_wasm_inference_and_catalog_vectors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/official-1.1.83.json")).unwrap();
        let mut vm = Vm::new().unwrap();
        vm.store.data_mut().fixture_counter = Some(0);
        let mut signer = Signer::with_vm(
            vm,
            "fixture-machine",
            crate::CLI_VERSION,
            fixture["identity"].clone(),
        )
        .unwrap();
        let infer = signer
            .inference(
                "https://api3.qoder.sh",
                fixture["body"].as_str().unwrap(),
                "fixture-model",
            )
            .unwrap();
        let catalog = signer
            .request(
                "https://api3.qoder.sh",
                "/api/v2/model/list",
                "GET",
                Some("{}"),
            )
            .unwrap();
        for (actual, key) in [(infer, "inference"), (catalog, "catalog")] {
            assert_eq!(actual.url, fixture[key]["url"]);
            assert_eq!(actual.body.unwrap(), fixture[key]["body"]);
            assert_eq!(
                serde_json::to_value(actual.headers).unwrap(),
                fixture[key]["headers"]
            );
        }
    }
    #[test]
    fn signer_bounds_input_and_releases_wasm_bindgen_objects() {
        let mut signer = Signer::new(
            "m",
            crate::CLI_VERSION,
            json!({"uid":"u","security_oauth_token":"fake"}),
        )
        .unwrap();
        for _ in 0..100 {
            signer
                .inference("https://api3.qoder.sh", "{}", "model")
                .unwrap();
        }
        assert!(signer.vm.store.data().heap.len() < 1200);
        assert!(signer
            .inference("https://api3.qoder.sh", &"x".repeat(MAX_BYTES + 1), "model")
            .is_err());
        assert!(signer
            .inference("https://api3.qoder.sh", "{}", "model")
            .is_ok());
    }
    #[test]
    fn plaintext_is_preserved_and_bad_ciphertext_fails_explicitly() {
        for text in ["{}", "[DONE]", "", " {\"statusCodeValue\":200}"] {
            assert_eq!(decrypt(text).unwrap(), text);
        }
        assert!(decrypt("invalid ciphertext").is_err());
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/official-1.1.83.json")).unwrap();
        assert_eq!(
            decrypt(fixture["inference"]["body"].as_str().unwrap()).unwrap(),
            fixture["body"]
        );
    }
}
