//! Build the unchanged 4.15.15 native source with BoringSSL's supported symbol
//! prefix headers. Read target archives in Rust, including ELF, Mach-O and COFF;
//! no host nm/objcopy or linker flags that discard duplicate definitions are used.
use crate::config::Config;
use object::{Object, ObjectSymbol};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const PREFIX: &str = "abv_bssl";
const ARCHIVE: &[u8] = include_bytes!("../vendor/boring-sys2-4.15.15.crate");
const CHECKSUM: &str = "ab6c876a958d17057d9d3a31075bb9609237413a1fe665432782c31ef596eb6b";

pub fn unpack(config: &Config) -> PathBuf {
    static SOURCE: OnceLock<PathBuf> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            assert_eq!(
                format!("{:x}", Sha256::digest(ARCHIVE)),
                CHECKSUM,
                "BoringSSL source integrity mismatch"
            );
            let directory = config.out_dir.join("upstream");
            fs::create_dir_all(&directory).unwrap();
            tar::Archive::new(flate2::read::GzDecoder::new(ARCHIVE))
                .unpack(&directory)
                .unwrap();
            directory.join("boring-sys2-4.15.15")
        })
        .clone()
}

pub fn configure(source: &Path) {
    let cmake = source.join("CMakeLists.txt");
    let original = fs::read_to_string(&cmake).unwrap();
    let extra = r#"
if(ABV_PREFIX_SYMBOLS)
  add_definitions(-DBORINGSSL_PREFIX=abv_bssl)
  include_directories("${CMAKE_CURRENT_SOURCE_DIR}/src/include")
endif()
"#;
    fs::write(
        cmake,
        original.replacen(
            "project(BoringSSL LANGUAGES C CXX)",
            &format!("project(BoringSSL LANGUAGES C CXX)\n{extra}"),
            1,
        ),
    )
    .unwrap();
}

fn archives(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, output: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), output);
            } else if matches!(
                entry.file_name().to_str(),
                Some("libssl.a" | "libcrypto.a" | "ssl.lib" | "crypto.lib")
            ) {
                output.push(entry.path());
            }
        }
    }
    let mut output = Vec::new();
    walk(&root.join("build"), &mut output);
    assert_eq!(
        output.len(),
        2,
        "Expected one native ssl and crypto archive"
    );
    output
}
fn symbols(config: &Config, root: &Path) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    for path in archives(root) {
        let bytes = fs::read(path).unwrap();
        let archive = object::read::archive::ArchiveFile::parse(bytes.as_slice()).unwrap();
        for member in archive.members() {
            let member = member.unwrap();
            let data = member.data(bytes.as_slice()).unwrap();
            let object = object::File::parse(data).expect("Unsupported native object format");
            for symbol in object.symbols() {
                if !symbol.is_global() || symbol.is_undefined() {
                    continue;
                }
                let name = symbol.name().expect("Invalid native symbol");
                let name = if config.target_os == "macos" || config.target_os == "ios" {
                    name.strip_prefix('_').unwrap_or(name)
                } else {
                    name
                };
                // C++ mangled/COMDAT symbols are not OpenSSL's C ABI and cannot
                // be renamed using preprocessor identifiers.
                if name.starts_with("_Z")
                    || name.is_empty()
                    || name
                        .bytes()
                        .any(|c| !c.is_ascii_alphanumeric() && c != b'_')
                {
                    continue;
                }
                if name.as_bytes()[0].is_ascii_digit() {
                    continue;
                }
                output.insert(name.to_owned());
            }
        }
    }
    output
}

pub fn generate(config: &Config, output: &Path, source: &Path) {
    let names = symbols(config, output);
    assert!(
        names.contains("SSL_new") && names.contains("BIO_new"),
        "Missing native TLS exports"
    );
    fs::write(
        config.out_dir.join("unprefixed-symbols.txt"),
        names.iter().cloned().collect::<Vec<_>>().join("\n"),
    )
    .unwrap();
    let mut c = String::from("// Generated from the current target's native archives.\n#define ABV_JOIN_INNER(a, b) a ## b\n#define ABV_JOIN(a, b) ABV_JOIN_INNER(a, b)\n");
    let mut asm = c.clone();
    let mut nasm = String::from("; Generated from the current target's native archives.\n");
    for name in &names {
        c.push_str(&format!("#define {name} {PREFIX}_{name}\n"));
        asm.push_str(&format!("#if defined(__APPLE__)\n#define _{name} _{PREFIX}_{name}\n#else\n#define {name} {PREFIX}_{name}\n#endif\n"));
        nasm.push_str(&format!("%ifidn __OUTPUT_FORMAT__,win32\n%define _{name} _{PREFIX}_{name}\n%else\n%define {name} {PREFIX}_{name}\n%endif\n"));
    }
    let include = source.join("src/include");
    for (file, contents) in [
        ("boringssl_prefix_symbols.h", c),
        ("boringssl_prefix_symbols_asm.h", asm),
        ("boringssl_prefix_symbols_nasm.inc", nasm),
    ] {
        fs::write(include.join(file), contents).unwrap();
    }
}

pub fn verify(config: &Config, output: &Path) {
    let names = symbols(config, output);
    let original = fs::read_to_string(config.out_dir.join("unprefixed-symbols.txt")).unwrap();
    let remaining: Vec<_> = original
        .lines()
        .filter(|name| names.contains(*name))
        .collect();
    assert!(
        remaining.is_empty(),
        "Unisolated BoringSSL exports: {remaining:?}"
    );
    assert!(names.contains("abv_bssl_SSL_new") && names.contains("abv_bssl_BIO_new"));
}

#[derive(Debug)]
pub struct Bindings(BTreeSet<String>);
impl Bindings {
    pub fn new(config: &Config) -> Self {
        Self(
            fs::read_to_string(config.out_dir.join("unprefixed-symbols.txt"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect(),
        )
    }
}
impl bindgen::callbacks::ParseCallbacks for Bindings {
    fn generated_link_name_override(
        &self,
        item: bindgen::callbacks::ItemInfo<'_>,
    ) -> Option<String> {
        self.0
            .contains(item.name)
            .then(|| format!("{PREFIX}_{}", item.name))
    }
}
