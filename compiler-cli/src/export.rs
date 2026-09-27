// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2022 The Gleam contributors

use crate::{
    command_build,
    fs::{self, ZipArchive, get_os},
};
use camino::Utf8PathBuf;
use clap::ValueEnum;
use gleam_core::build::Telemetry;
use gleam_core::{
    Result,
    analyse::TargetSupport,
    build::{Codegen, Compile, ErlangOutput, Mode, Options, Target},
    docs,
    error::ShellCommandFailureReason,
    io,
    paths::ProjectPaths,
    type_::ModuleFunction,
};
use serde::{Deserialize, Serialize};
use std::{io::Cursor, time::Instant};
use strum::{Display, EnumString, VariantNames};

static ENTRYPOINT_FILENAME_POWERSHELL: &str = "entrypoint.ps1";
static ENTRYPOINT_FILENAME_POSIX_SHELL: &str = "entrypoint.sh";

static ENTRYPOINT_TEMPLATE_POWERSHELL: &str =
    include_str!("../templates/erlang-shipment-entrypoint.ps1");
static ENTRYPOINT_TEMPLATE_POSIX_SHELL: &str =
    include_str!("../templates/erlang-shipment-entrypoint.sh");

/// Generate a single file of precompiled Erlang, suitable for CLIs.
///
pub fn escript(paths: &ProjectPaths) -> Result<()> {
    let target = Target::Erlang;
    let mode = Mode::Prod;
    let build = paths.build_directory_for_target(mode, target);

    // Reset the directories to ensure we have a clean slate and no old code
    fs::delete_directory(&build)?;

    let manifest = crate::build::download_dependencies(paths, crate::cli::Reporter::new())?;

    // Build project in production mode
    let build_options = Options {
        root_target_support: TargetSupport::Enforced,
        warnings_as_errors: false,
        codegen: Codegen::All,
        compile: Compile::All,
        mode,
        target: Some(target),
        no_print_progress: false,
        erlang_output: ErlangOutput::Binary,
    };
    let built = crate::build::main(paths, build_options, manifest)?;
    let package_name = &built.root_package.config.name;

    // The main function must exist for the escript to call. This will return an
    // error if it could not be found.
    let _: ModuleFunction = built.get_main_function(package_name, target)?;

    // Create the zip archive for the code
    let mut zip = ZipArchive::new(Cursor::new(Vec::new()));

    for entry in fs::read_dir(&build)?.filter_map(Result::ok) {
        let ebin = entry.path().join("ebin");

        // We want the ebin code directories for each package
        if !ebin.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&ebin)?.filter_map(Result::ok) {
            let path = entry.path();
            let extension = path.extension().unwrap_or_default();

            let Some(name) = path.file_name() else {
                continue;
            };

            if !path.is_file() {
                continue;
            }

            // We want to copy compiled BEAM bytecode and app configuration files
            if extension != "beam" && extension != "app" {
                continue;
            }

            zip.add_file_from_disc(path, name)?;
        }
    }

    let zip = zip.finish()?.into_inner();

    let escript_path = paths.root().join(package_name.as_str());
    let mut file = fs::open_file(&escript_path)?;

    // The -escript flag in the header instructs the BEAM `escript` program
    // to run the regular Gleam entrypoint module when running this escript.
    let header = format!(
        "#!/usr/bin/env escript
%%
%%!-escript main {package_name}@@main
"
    );

    fs::write_to_open_file(&mut file, &escript_path, header)?;
    fs::write_to_open_file(&mut file, &escript_path, zip)?;
    fs::make_executable(&escript_path)?;

    // Windows shells largely do not use shebangs, so for the escript to be
    // directly executable a .cmd wrapper script is provided.
    if cfg!(windows) {
        let cmd_path = escript_path.with_extension("cmd");
        fs::write(&cmd_path, "@echo off\r\nescript.exe \"%~dpn0\" %*\r\n")?;
    }

    println!(
        "
Your escript has been generated to {escript_path}.
",
    );

    Ok(())
}

/// Generate a directory of precompiled Erlang along with a start script.
/// Suitable for deployment to a server.
///
/// For each Erlang application (aka package) directory these directories are
/// copied across:
/// - ebin
/// - include
/// - priv
pub(crate) fn erlang_shipment(paths: &ProjectPaths) -> Result<()> {
    let target = Target::Erlang;
    let mode = Mode::Prod;
    let build = paths.build_directory_for_target(mode, target);
    let out = paths.erlang_shipment_directory();

    fs::mkdir(&out)?;

    // Reset the directories to ensure we have a clean slate and no old code
    fs::delete_directory(&build)?;
    fs::delete_directory(&out)?;

    // Build project in production mode
    let built = crate::build::main(
        paths,
        Options {
            root_target_support: TargetSupport::Enforced,
            warnings_as_errors: false,
            codegen: Codegen::All,
            compile: Compile::All,
            mode,
            target: Some(target),
            no_print_progress: false,
            erlang_output: ErlangOutput::Binary,
        },
        crate::build::download_dependencies(paths, crate::cli::Reporter::new())?,
    )?;

    for entry in fs::read_dir(&build)?.filter_map(Result::ok) {
        let path = entry.path();

        // We are only interested in package directories
        if !path.is_dir() {
            continue;
        }

        let name = path.file_name().expect("Directory name");
        let build = build.join(name);
        let out = out.join(name);
        fs::mkdir(&out)?;

        // Copy desired package subdirectories
        for subdirectory in ["ebin", "priv", "include"] {
            let source = build.join(subdirectory);
            if source.is_dir() {
                let source = fs::canonicalise(&source)?;
                let out = out.join(subdirectory);
                fs::copy_dir(source, &out)?;
            }
        }
    }

    // PowerShell entry point script.
    write_entrypoint_script(
        &out.join(ENTRYPOINT_FILENAME_POWERSHELL),
        ENTRYPOINT_TEMPLATE_POWERSHELL,
        &built.root_package.config.name,
    )?;

    // POSIX Shell entry point script.
    write_entrypoint_script(
        &out.join(ENTRYPOINT_FILENAME_POSIX_SHELL),
        ENTRYPOINT_TEMPLATE_POSIX_SHELL,
        &built.root_package.config.name,
    )?;

    crate::cli::print_exported(&built.root_package.config.name);

    println!(
        "
Your Erlang shipment has been generated to {out}.

It can be copied to a compatible server with Erlang installed and run with
one of the following scripts:
    - {ENTRYPOINT_FILENAME_POWERSHELL} (PowerShell script)
    - {ENTRYPOINT_FILENAME_POSIX_SHELL} (POSIX Shell script)
",
    );

    Ok(())
}

fn write_entrypoint_script(
    entrypoint_output_path: &Utf8PathBuf,
    entrypoint_template_path: &str,
    package_name: &str,
) -> Result<()> {
    let text = entrypoint_template_path.replace("$PACKAGE_NAME_FROM_GLEAM", package_name);
    fs::write(entrypoint_output_path, &text)?;
    fs::make_executable(entrypoint_output_path)?;
    Ok(())
}

pub fn hex_tarball(paths: &ProjectPaths) -> Result<()> {
    let mut config = crate::config::root_config(paths)?;
    let data: Vec<u8> = crate::publish::build_hex_tarball(paths, &mut config)?;

    let path = paths.build_export_hex_tarball(&config.name, &config.version.to_string());
    fs::write_bytes(&path, &data)?;
    println!(
        "
Your hex tarball has been generated in {}.
",
        path
    );
    Ok(())
}

fn write_path_or_stdout(
    paths: &ProjectPaths,
    out: Option<Utf8PathBuf>,
    content: String,
) -> Result<()> {
    match out {
        Some(out) => {
            let out = io::OutputFile::text(out, content);
            fs::write_outputs_under(&[out], paths.root())?;
        }
        None => print!("{}", content),
    }
    Ok(())
}

pub fn javascript_prelude(paths: &ProjectPaths, out: Option<Utf8PathBuf>) -> Result<()> {
    write_path_or_stdout(paths, out, gleam_core::javascript::PRELUDE.into())
}

pub fn typescript_prelude(paths: &ProjectPaths, out: Option<Utf8PathBuf>) -> Result<()> {
    write_path_or_stdout(paths, out, gleam_core::javascript::PRELUDE_TS_DEF.into())
}

pub fn package_interface(paths: &ProjectPaths, out: Option<Utf8PathBuf>) -> Result<()> {
    // Build the project
    let mut built = crate::build::main(
        paths,
        Options {
            mode: Mode::Prod,
            target: None,
            codegen: Codegen::None,
            compile: Compile::All,
            warnings_as_errors: false,
            root_target_support: TargetSupport::Enforced,
            no_print_progress: false,
            erlang_output: ErlangOutput::Binary,
        },
        crate::build::download_dependencies(paths, crate::cli::Reporter::new())?,
    )?;
    built.root_package.attach_doc_and_module_comments();
    let interface = docs::package_interface(&built.root_package, &built.module_interfaces);
    write_path_or_stdout(paths, out, interface)
}

pub fn package_information(paths: &ProjectPaths, out: Option<Utf8PathBuf>) -> Result<()> {
    let config = crate::config::root_config(paths)?;
    let information = docs::package_information_as_json(config);
    write_path_or_stdout(paths, out, information)
}

fn command_output_to_str(output: std::process::Output) -> String {
    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        stdout.push('\n');
        stdout.push_str(&String::from_utf8_lossy(&output.stderr).to_string());
    }
    stdout
}

fn map_shell_error(program: String, e: std::io::Error) -> gleam_core::Error {
    match e.kind() {
        std::io::ErrorKind::NotFound => gleam_core::Error::ShellProgramNotFound {
            program,
            os: get_os(),
        },
        other => gleam_core::Error::ShellCommand {
            program,
            reason: ShellCommandFailureReason::IoError(other),
        },
    }
}

#[derive(
    Debug, Serialize, Deserialize, Display, EnumString, VariantNames, ValueEnum, Clone, Copy,
)]
#[strum(serialize_all = "lowercase")]
#[clap(rename_all = "lower")]
pub enum PorfforTarget {
    Native,
    WASM,
}

pub fn porffor(paths: &ProjectPaths, target: PorfforTarget, porf: Option<String>) -> Result<()> {
    let built = command_build(paths, Some(Target::JavaScript), false, false)?;
    let name = built.root_package.config.name.as_str();
    let telemetry = &crate::cli::Reporter;
    let start = Instant::now();

    let program_file = paths
        .build_directory_for_package(built.mode, Target::JavaScript, name)
        .join(format!("{}.mjs", name));

    let dest_dir = paths.build_directory_for_mode(built.mode).join("porffor");
    let mut entry_file = dest_dir.join("entry.mjs");

    // todo: I feel like I saw somewhere that gleam generates this?
    //       If so can we reuse that logic
    fs::write(
        &entry_file,
        &format!("import {{ main }} from \"{}\";\nmain();", program_file),
    )?;

    // I tried with rolldown to avoid the external esbuild dependency, but it added
    // too much to the binary size of gleam, so we will stick with esbuild for now
    //
    // let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");
    // let mut bundler = rolldown::Bundler::new(rolldown::BundlerOptions {
    //     input: Some(vec![entry_file.to_string().into()]),
    //     file: Some(bundle_file.to_string()),
    //     platform: Some(rolldown::Platform::Node),
    //     code_splitting: Some(rolldown::CodeSplittingMode::Bool(false)),
    //     ..Default::default()
    // })
    // .expect("failed to create rolldown bundler");
    //
    // let _ = runtime
    //     .block_on(bundler.write())
    //     .expect("failed to write bundle");

    // For now we can just say that if there is a custom porf binary
    // We should bundle. This entire step will likely be removed anyway
    // and was just added so I can benchmark old and new porffor versions
    if porf.is_some() {
        let bundle_file = dest_dir.join("bundle.mjs");

        let output = std::process::Command::new("esbuild")
            .arg("--bundle")
            .arg("--platform=node")
            .arg("--format=esm")
            .arg("--target=esnext")
            .arg(format!("--outfile={}", bundle_file.to_string()))
            .arg(&entry_file)
            .output()
            .map_err(|e| map_shell_error("esbuild".into(), e))?;

        if !output.status.success() {
            return Err(gleam_core::Error::EsbuildFailed {
                code: output.status.code(),
                error: command_output_to_str(output),
            });
        }

        entry_file = bundle_file;
        telemetry.bundled_js(start.elapsed());
    }

    let porffor_output = match target {
        PorfforTarget::Native => dest_dir.join(name),
        PorfforTarget::WASM => dest_dir.join(format!("{}.c", name)),
    };

    let porf = porf.unwrap_or("porf".into());

    let output = std::process::Command::new(&porf)
        .arg(match target {
            PorfforTarget::Native => "native",
            PorfforTarget::WASM => "c",
        })
        .arg("--module")
        .arg(entry_file)
        .arg(format!("-o={}", &porffor_output))
        .output()
        .map_err(|e| map_shell_error(porf, e))?;

    if !output.status.success() {
        return Err(gleam_core::Error::PorfforFailed {
            code: output.status.code(),
            error: command_output_to_str(output),
        });
    }

    telemetry.porffored(start.elapsed());

    if matches!(target, PorfforTarget::WASM) {
        let wasi_sdk_output = dest_dir.join(format!("{}.wasm", name));

        let output = std::process::Command::new("/opt/wasi-sdk/bin/clang")
            // Porffor C output
            .arg(porffor_output)
            // Output WASM file
            .arg("-o")
            .arg(wasi_sdk_output)
            // Compile to WASM target
            .arg("--target=wasm32-wasip1")
            // WASM has no signal support, so use minimal signal emulation
            .arg("-D_WASI_EMULATED_SIGNAL")
            .arg("-lwasi-emulated-signal")
            // Enable non-yet-standardised Exception handling
            .arg("-mllvm")
            .arg("-wasm-enable-sjlj")
            .arg("-lsetjmp")
            .arg("-mllvm")
            .arg("-wasm-use-legacy-eh=false")
            // WASM lacks a true mmap, so use a minimal mmap emulation
            .arg("-D_WASI_EMULATED_MMAN")
            .arg("-lwasi-emulated-mman")
            // Optimisation
            .arg("-O2")
            .output()
            .map_err(|e| map_shell_error("/opt/wasi-sdk/bin/clang".into(), e))?;

        if !output.status.success() {
            return Err(gleam_core::Error::WasmificationFailed {
                code: output.status.code(),
                error: command_output_to_str(output),
            });
        }

        telemetry.wasmified(start.elapsed());
    }

    Ok(())
}
