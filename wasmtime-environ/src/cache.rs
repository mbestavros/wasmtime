use crate::address_map::{ModuleAddressMap, ValueLabelsRanges};
use crate::compilation::{Compilation, Relocations};
use crate::module::Module;
use crate::module_environ::FunctionBodyData;
use core::hash::Hasher;
use cranelift_codegen::{ir, isa};
use cranelift_entity::PrimaryMap;
use cranelift_wasm::DefinedFuncIndex;
use lazy_static::lazy_static;
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::string::{String, ToString};

/// Module for configuring the cache system.
pub mod conf {
    use directories::ProjectDirs;
    use log::{debug, warn};
    use spin::Once;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Config {
        pub cache_enabled: bool,
        pub cache_dir: PathBuf,
        pub compression_level: i32,
    }

    // Private static, so only internal function can access it.
    static CONFIG: Once<Config> = Once::new();
    static INIT_CALLED: AtomicBool = AtomicBool::new(false);
    static DEFAULT_COMPRESSION_LEVEL: i32 = 0; // 0 for zstd means "use default level"

    /// Returns true if and only if the cache is enabled.
    pub fn cache_enabled() -> bool {
        // Not everyone knows about the cache system, i.e. the tests,
        // so the default is cache disabled.
        CONFIG
            .call_once(|| Config::new_cache_disabled())
            .cache_enabled
    }

    /// Returns path to the cache directory.
    ///
    /// Panics if the cache is disabled.
    pub fn cache_directory() -> &'static PathBuf {
        &CONFIG
            .r#try()
            .expect("Cache system must be initialized")
            .cache_dir
    }

    /// Returns cache compression level.
    ///
    /// Panics if the cache is disabled.
    pub fn compression_level() -> i32 {
        CONFIG
            .r#try()
            .expect("Cache system must be initialized")
            .compression_level
    }

    /// Initializes the cache system. Should be called exactly once,
    /// and before using the cache system. Otherwise it can panic.
    pub fn init<P: AsRef<Path>>(enabled: bool, dir: Option<P>, compression_level: Option<i32>) {
        INIT_CALLED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .expect("Cache system init must be called at most once");
        assert!(
            CONFIG.r#try().is_none(),
            "Cache system init must be called before using the system."
        );
        let conf = CONFIG.call_once(|| {
            Config::new(
                enabled,
                dir,
                compression_level.unwrap_or(DEFAULT_COMPRESSION_LEVEL),
            )
        });
        debug!(
            "Cache init(): enabled={}, cache-dir={:?}, compression-level={}",
            conf.cache_enabled, conf.cache_dir, conf.compression_level,
        );
    }

    impl Config {
        pub fn new_cache_disabled() -> Self {
            Self {
                cache_enabled: false,
                cache_dir: PathBuf::new(),
                compression_level: DEFAULT_COMPRESSION_LEVEL,
            }
        }

        pub fn new<P: AsRef<Path>>(enabled: bool, dir: Option<P>, compression_level: i32) -> Self {
            if enabled {
                match dir {
                    Some(dir) => Self::new_step2(dir.as_ref(), compression_level),
                    None => match ProjectDirs::from("", "CraneStation", "wasmtime") {
                        Some(proj_dirs) => {
                            Self::new_step2(proj_dirs.cache_dir(), compression_level)
                        }
                        None => {
                            warn!("Cache directory not specified and failed to find the default. Disabling cache.");
                            Self::new_cache_disabled()
                        }
                    },
                }
            } else {
                Self::new_cache_disabled()
            }
        }

        fn new_step2(dir: &Path, compression_level: i32) -> Self {
            // On Windows, if we want long paths, we need '\\?\' prefix, but it doesn't work
            // with relative paths. One way to get absolute path (the only one?) is to use
            // fs::canonicalize, but it requires that given path exists. The extra advantage
            // of this method is fact that the method prepends '\\?\' on Windows.
            match fs::create_dir_all(dir) {
                Ok(()) => match fs::canonicalize(dir) {
                    Ok(p) => Self {
                        cache_enabled: true,
                        cache_dir: p,
                        compression_level,
                    },
                    Err(err) => {
                        warn!(
                            "Failed to canonicalize the cache directory. Disabling cache. \
                             Message: {}",
                            err
                        );
                        Self::new_cache_disabled()
                    }
                },
                Err(err) => {
                    warn!(
                        "Failed to create the cache directory. Disabling cache. Message: {}",
                        err
                    );
                    Self::new_cache_disabled()
                }
            }
        }
    }
}

lazy_static! {
    static ref SELF_MTIME: String = {
        std::env::current_exe()
            .map_err(|_| warn!("Failed to get path of current executable"))
            .ok()
            .and_then(|path| {
                fs::metadata(&path)
                    .map_err(|_| warn!("Failed to get metadata of current executable"))
                    .ok()
            })
            .and_then(|metadata| {
                metadata
                    .modified()
                    .map_err(|_| warn!("Failed to get metadata of current executable"))
                    .ok()
            })
            .and_then(|mtime| {
                Some(match mtime.duration_since(std::time::UNIX_EPOCH) {
                    Ok(duration) => format!("{}", duration.as_millis()),
                    Err(err) => format!("m{}", err.duration().as_millis()),
                })
            })
            .unwrap_or("no-mtime".to_string())
    };
}

pub struct ModuleCacheEntry {
    mod_cache_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct ModuleCacheData {
    compilation: Compilation,
    relocations: Relocations,
    address_transforms: ModuleAddressMap,
    value_ranges: ValueLabelsRanges,
    stack_slots: PrimaryMap<DefinedFuncIndex, ir::StackSlots>,
}

type ModuleCacheDataTupleType = (
    Compilation,
    Relocations,
    ModuleAddressMap,
    ValueLabelsRanges,
    PrimaryMap<DefinedFuncIndex, ir::StackSlots>,
);

struct Sha256Hasher(Sha256);

impl ModuleCacheEntry {
    pub fn new<'data>(
        module: &Module,
        function_body_inputs: &PrimaryMap<DefinedFuncIndex, FunctionBodyData<'data>>,
        isa: &dyn isa::TargetIsa,
        compiler_name: &str,
        generate_debug_info: bool,
    ) -> Self {
        let mod_cache_path = if conf::cache_enabled() {
            let hash = Sha256Hasher::digest(module, function_body_inputs);
            let compiler_dir = if cfg!(debug_assertions) {
                format!(
                    "{comp_name}-{comp_ver}-{comp_mtime}",
                    comp_name = compiler_name,
                    comp_ver = env!("GIT_REV"),
                    comp_mtime = *SELF_MTIME,
                )
            } else {
                format!(
                    "{comp_name}-{comp_ver}",
                    comp_name = compiler_name,
                    comp_ver = env!("GIT_REV"),
                )
            };
            let mod_filename = format!(
                "mod-{mod_hash}{mod_dbg}",
                mod_hash = base64::encode_config(&hash, base64::URL_SAFE_NO_PAD), // standard encoding uses '/' which can't be used for filename
                mod_dbg = if generate_debug_info { ".d" } else { "" },
            );
            Some(
                conf::cache_directory()
                    .join(isa.triple().to_string())
                    .join(compiler_dir)
                    .join(mod_filename),
            )
        } else {
            None
        };

        Self { mod_cache_path }
    }

    pub fn get_data(&self) -> Option<ModuleCacheData> {
        let path = self.mod_cache_path.as_ref()?;
        trace!("get_data() for path: {}", path.display());
        let compressed_cache_bytes = fs::read(path).ok()?;
        let cache_bytes = zstd::decode_all(&compressed_cache_bytes[..])
            .map_err(|err| warn!("Failed to decompress cached code: {}", err))
            .ok()?;
        bincode::deserialize(&cache_bytes[..])
            .map_err(|err| warn!("Failed to deserialize cached code: {}", err))
            .ok()
    }

    pub fn update_data(&self, data: &ModuleCacheData) {
        let _ = self.update_data_impl(data);
    }

    fn update_data_impl(&self, data: &ModuleCacheData) -> Option<()> {
        let path = self.mod_cache_path.as_ref()?;
        trace!("update_data() for path: {}", path.display());
        let serialized_data = bincode::serialize(&data)
            .map_err(|err| warn!("Failed to serialize cached code: {}", err))
            .ok()?;
        let compressed_data = zstd::encode_all(&serialized_data[..], conf::compression_level())
            .map_err(|err| warn!("Failed to compress cached code: {}", err))
            .ok()?;

        // Optimize syscalls: first, try writing to disk. It should succeed in most cases.
        // Otherwise, try creating the cache directory and retry writing to the file.
        let err = fs::write(path, &compressed_data).err()?; // return on success
        debug!(
            "Attempting to create the cache directory, because \
             failed to write cached code to disk, path: {}, message: {}",
            path.display(),
            err,
        );

        let cache_dir = path.parent().unwrap();
        fs::create_dir_all(cache_dir)
            .map_err(|err| {
                warn!(
                    "Failed to create cache directory, path: {}, message: {}",
                    cache_dir.display(),
                    err
                )
            })
            .ok()?;

        let err = fs::write(path, &compressed_data).err()?;
        warn!(
            "Failed to write cached code to disk, path: {}, message: {}",
            path.display(),
            err
        );
        fs::remove_file(path)
            .map_err(|err| {
                if err.kind() != io::ErrorKind::NotFound {
                    warn!(
                        "Failed to cleanup invalid cache, path: {}, message: {}",
                        path.display(),
                        err
                    );
                }
            })
            .ok()
    }
}

impl ModuleCacheData {
    pub fn from_tuple(data: ModuleCacheDataTupleType) -> Self {
        Self {
            compilation: data.0,
            relocations: data.1,
            address_transforms: data.2,
            value_ranges: data.3,
            stack_slots: data.4,
        }
    }

    pub fn to_tuple(self) -> ModuleCacheDataTupleType {
        (
            self.compilation,
            self.relocations,
            self.address_transforms,
            self.value_ranges,
            self.stack_slots,
        )
    }
}

impl Sha256Hasher {
    pub fn digest<'data>(
        module: &Module,
        function_body_inputs: &PrimaryMap<DefinedFuncIndex, FunctionBodyData<'data>>,
    ) -> [u8; 32] {
        let mut hasher = Self(Sha256::new());
        module.hash_for_cache(function_body_inputs, &mut hasher);
        hasher.0.result().into()
    }
}

impl Hasher for Sha256Hasher {
    fn finish(&self) -> u64 {
        panic!("Sha256Hasher doesn't support finish!");
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.input(bytes);
    }
}

#[cfg(test)]
mod tests;
