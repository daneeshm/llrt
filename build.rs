use std::{
    collections::HashSet,
    env,
    error::Error,
    fs::{self, File},
    io::{self, BufWriter},
    path::{Path, PathBuf, MAIN_SEPARATOR_STR},
    process::Command,
    result::Result as StdResult,
};

use std::io::Write;

use jwalk::WalkDir;
use rquickjs::{
    loader::{Loader, Resolver},
    module::ModuleData,
    CatchResultExt, Context, Ctx, Module, Runtime,
};

const BUNDLE_DIR: &str = "bundle";
#[cfg(feature = "uncompressed")]
const BYTECODE_EXT: &str = "lrtu";
#[cfg(not(feature = "uncompressed"))]
const BYTECODE_EXT: &str = "lrt";

include!("src/bytecode_meta.rs");

macro_rules! info {
    ($($tokens: tt)*) => {
        println!("cargo:info={}", format!($($tokens)*))
    }
}

macro_rules! rerun_if_changed {
    ($file: expr) => {
        println!("cargo:rerun-if-changed={}", $file)
    };
}

struct DummyLoader;

impl Loader for DummyLoader {
    fn load(&mut self, _ctx: &Ctx<'_>, name: &str) -> rquickjs::Result<ModuleData> {
        Ok(ModuleData::source(name, ""))
    }
}

struct DummyResolver;

impl Resolver for DummyResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, _base: &str, name: &str) -> rquickjs::Result<String> {
        Ok(name.into())
    }
}

fn human_file_size(size: usize) -> String {
    let fsize = size as f64;
    let i = if size == 0 {
        0
    } else {
        (fsize.log2() / 1024f64.log2()).floor() as i32
    };
    let size = fsize / 1024f64.powi(i);
    let units = ["B", "kB", "MB", "GB", "TB", "PB"];
    format!("{:.3} {}", size, units[i as usize])
}

#[tokio::main]
async fn main() -> StdResult<(), Box<dyn Error>> {
    rerun_if_changed!(BUNDLE_DIR);

    let resolver = (DummyResolver,);
    let loader = (DummyLoader,);

    let rt = Runtime::new().unwrap();
    rt.set_loader(resolver, loader);
    let ctx = Context::full(&rt).unwrap();

    let sdk_bytecode_path = Path::new("src").join("bytecode_cache.rs");
    let mut sdk_bytecode_file = BufWriter::new(File::create(sdk_bytecode_path).unwrap());

    let mut ph_map = phf_codegen::Map::<String>::new();
    let mut filenames = vec![];
    let mut total_bytes: usize = 0;

    fs::write("VERSION", env!("CARGO_PKG_VERSION")).expect("Unable to write VERSION file");

    ctx.with(|ctx| {
        let mut compile = |ctx: Ctx<'_>| {
            for dir_ent in WalkDir::new(BUNDLE_DIR).into_iter().flatten() {
                let path = dir_ent.path();

                let path = path.strip_prefix(BUNDLE_DIR).unwrap().to_owned();
                let path_str = path.to_string_lossy().to_string();

                if path_str.starts_with("__tests__") || path.extension().unwrap_or_default() != "js"
                {
                    continue;
                }

                #[cfg(feature = "lambda")]
                {
                    if path == PathBuf::new().join("@llrt").join("test.js") {
                        continue;
                    }
                }

                #[cfg(feature = "no-sdk")]
                {
                    if path_str.starts_with("@aws-sdk")
                        || path_str.starts_with("@smithy")
                        || path_str.starts_with("llrt-chunk-sdk")
                    {
                        continue;
                    }
                }

                let source = fs::read_to_string(dir_ent.path()).unwrap_or_else(|_| {
                    panic!("Unable to load: {}", dir_ent.path().to_string_lossy())
                });

                let module_name = if !path_str.starts_with("llrt-chunk-") {
                    path.with_extension("").to_string_lossy().to_string()
                } else {
                    path.to_string_lossy().to_string()
                };

                info!("Compiling module: {}", module_name);

                let filename = dir_ent
                    .path()
                    .with_extension(BYTECODE_EXT)
                    .to_string_lossy()
                    .to_string();
                filenames.push(filename.clone());
                let ctx2 = ctx.clone();
                let bytes = move || {
                    {
                        let module = unsafe {
                            Module::unsafe_declare(ctx2.clone(), module_name.clone(), source)
                        }?;
                        module.write_object(false)
                    }()
                    .catch(&ctx)?;
        
                total_bytes += bytes.len();

                if cfg!(feature = "uncompressed") {
                    let mut uncompressed = Vec::with_capacity(4 + 6 + bytes.len());
                    uncompressed.extend_from_slice(BYTECODE_VERSION.as_bytes());
                    uncompressed.extend_from_slice(&[BYTECODE_UNCOMPRESSED]); //uncompressed
                    uncompressed.extend_from_slice(&bytes);
                    fs::write(&filename, uncompressed).unwrap();
                } else {
                    fs::write(&filename, bytes).unwrap();
                }

                info!("Done!");

                ph_map.entry(
                    module_name,
                    &format!("include_bytes!(\"..{}{}\")", MAIN_SEPARATOR_STR, &filename),
                );
            }
            Ok::<_, Box<dyn Error>>(())
        };
        compile(ctx)
    })?;

    write!(
        &mut sdk_bytecode_file,
        "// @generated by build.rs\n\npub static BYTECODE_CACHE: phf::Map<&'static str, &[u8]> = {}",
        ph_map.build()
    )?;
    writeln!(&mut sdk_bytecode_file, ";")?;

    info!(
        "\n===============================\nUncompressed bytecode size: {}\n===============================",
        human_file_size(total_bytes)
    );

    let bundle_path = Path::new(BUNDLE_DIR);
    let compression_dictionary_path = bundle_path
        .join("compression.dict")
        .to_string_lossy()
        .to_string();

    if cfg!(feature = "uncompressed") {
        fs::write(compression_dictionary_path, "")?;
    } else {
        total_bytes =
            compress_bytecode(bundle_path, compression_dictionary_path, filenames).unwrap();

        info!(
            "\n===============================\nCompressed bytecode size: {}\n===============================",
            human_file_size(total_bytes)
        );
    }

    Ok(())
}

fn compress_bytecode(
    bundles_path: &Path,
    dictionary_path: String,
    source_files: Vec<String>,
) -> io::Result<usize> {
    info!("Generating compression dictionary...");

    let file_count = source_files.len();
    let mut dictionary_filenames = source_files.clone();
    let mut dictionary_file_set: HashSet<String> = HashSet::from_iter(dictionary_filenames.clone());

    let mut cmd = Command::new("zstd");
    cmd.args([
        "--train",
        "--train-fastcover=steps=40",
        "--maxdict=20K",
        "-o",
        &dictionary_path,
    ]);
    if file_count < 5 {
        dictionary_file_set.retain(|file_path| {
            let metadata = fs::metadata(file_path).unwrap();
            let file_size = metadata.len();
            file_size >= 1024 // 1 kilobyte = 1024 bytes
        });
        cmd.arg("-B1K");
        dictionary_filenames = dictionary_file_set.into_iter().collect();
    }
    cmd.args(&dictionary_filenames);

    let mut cmd = cmd.args(&source_files).spawn()?;

    let exit_status = cmd.wait()?;

    if !exit_status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to generate compression dictionary",
        ));
    }

    let mut total_size = 0;
    let tmp_dir = env::temp_dir();

    for filename in source_files {
        info!("Compressing {}...", filename);

        let tmp_filename = tmp_dir
            .join(nanoid::nanoid!())
            .to_string_lossy()
            .to_string();

        fs::copy(&filename, &tmp_filename)?;

        let uncompressed_file_size = PathBuf::from(&filename).metadata().unwrap().len() as u32;

        let output = Command::new("zstd")
            .args([
                "--ultra",
                "-22",
                "-f",
                "-D",
                &dictionary_path,
                &tmp_filename,
                "-o",
                &filename,
            ])
            .output()?;

        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to compress file",
            ));
        }

        let bytes = fs::read(&filename)?;
        let mut compressed = Vec::with_capacity(4 + 6 + bytes.len());
        compressed.extend_from_slice(BYTECODE_VERSION.as_bytes());
        compressed.extend_from_slice(&[BYTECODE_COMPRESSED]); //compressed
        compressed.extend_from_slice(&uncompressed_file_size.to_le_bytes());
        compressed.extend_from_slice(&bytes);
        fs::write(&filename, compressed)?;

        let compressed_file_size = PathBuf::from(&filename).metadata().unwrap().len() as usize;

        total_size += compressed_file_size;
    }

    Ok(total_size)
}
