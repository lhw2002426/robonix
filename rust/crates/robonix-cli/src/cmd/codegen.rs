// SPDX-License-Identifier: MulanPSL-2.0
// `rbnx codegen -p <package>` — one-shot codegen wrapper that replaces
// the copy-pasted robonix-codegen + grpc_tools.protoc boilerplate in each
// package's build.sh.
//
// For a given package root:
//   1. Regenerate `<robonix>/rust/crates/robonix-interfaces/robonix_proto/`
//      from system IDL + contracts (robonix-codegen --lang proto).
//   2. If --mcp: regenerate `<pkg>/robonix_mcp_types/`
//      (robonix-codegen --lang mcp).
//   3. Regenerate `<pkg>/proto_gen/` via grpc_tools.protoc, covering the
//      runtime proto + all of robonix_proto/*.proto.
//   4. Write `<pkg>/rbnx-build/ws/install/setup.bash` so `rbnx start`
//      injects the right PYTHONPATH.
//
// TODO: union package-local contracts (`<pkg>/contracts/`) and package-local
// IDL (`<pkg>/interfaces/lib/`) into a staging dir before running codegen —
// currently those are a copy-pasted snippet in a few build.sh scripts.

use anyhow::{Context, Result};
use colored::*;
use robonix_cli::{Config, SourcePathKey};
use robonix_cli::workspace::{WorkspaceConfig, ensure_packages_exist};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_cmd(label: &str, cmd: &mut Command) -> Result<()> {
    log::debug!("[codegen] {}: {:?}", label, cmd);
    let status = cmd
        .status()
        .with_context(|| format!("failed to execute `{label}`"))?;
    if !status.success() {
        anyhow::bail!("{label} failed with {status}");
    }
    Ok(())
}

fn resolve_pkg_root(package: &Path) -> Result<PathBuf> {
    let abs = if package.is_absolute() {
        package.to_path_buf()
    } else {
        let base = std::env::var("RBNX_INVOCATION_CWD")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        base.join(package)
    };
    let abs = abs
        .canonicalize()
        .with_context(|| format!("package path not found: {}", abs.display()))?;
    if !abs.join("robonix_manifest.yaml").exists() && !abs.join("rbnx_manifest.yaml").exists() {
        eprintln!(
            "{}: {} has no robonix_manifest.yaml (continuing anyway)",
            "warn".yellow().bold(),
            abs.display()
        );
    }
    Ok(abs)
}

pub async fn execute(
    config: Config,
    package: Option<PathBuf>,
    mcp: bool,
    clean: bool,
    out_dir: Option<PathBuf>,
) -> Result<()> {
    if let Some(pkg_path) = package {
        // Single-package mode (original behavior).
        execute_single(&config, &pkg_path, mcp, clean, out_dir.as_deref())
    } else {
        // Workspace mode: run codegen for all packages in robonix_workspace.yaml.
        execute_workspace(&config, mcp, clean).await
    }
}

/// Workspace-level codegen: reads robonix_workspace.yaml and runs codegen for all packages.
async fn execute_workspace(config: &Config, mcp: bool, clean: bool) -> Result<()> {
    let ws_yaml = PathBuf::from("robonix_workspace.yaml");
    if !ws_yaml.exists() {
        anyhow::bail!(
            "no robonix_workspace.yaml found in current directory; \
             use 'rbnx codegen -p <package>' to codegen a single package"
        );
    }

    let workspace_root = std::env::current_dir()?;
    let content = std::fs::read_to_string(&ws_yaml)?;
    let ws: WorkspaceConfig = serde_yaml::from_str(&content)
        .with_context(|| "failed to parse robonix_workspace.yaml")?;

    println!(
        "{} workspace codegen: {} ({} package(s))",
        "[codegen]".bold(),
        ws.workspace.as_deref().unwrap_or("unnamed"),
        ws.packages.len(),
    );

    if ws.packages.is_empty() {
        println!("{} no packages declared — nothing to do", "[codegen]".yellow().bold());
        return Ok(());
    }

    // Resolve package paths (with git clone support).
    let package_paths = ensure_packages_exist(&workspace_root, &ws.packages)?;

    // Run codegen for each package.
    let mut succeeded = 0usize;
    for pkg_entry in &ws.packages {
        let pkg_path = package_paths.get(&pkg_entry.name).unwrap();
        println!(
            "\n{} [{}/{}] codegen for {} ({})",
            "[codegen]".bold(),
            succeeded + 1,
            ws.packages.len(),
            pkg_entry.name,
            pkg_path.display(),
        );
        match execute_single(config, pkg_path, mcp, clean, None) {
            Ok(()) => succeeded += 1,
            Err(e) => {
                eprintln!(
                    "{} codegen failed for '{}': {e}",
                    "error".red().bold(),
                    pkg_entry.name
                );
                // Continue with remaining packages (best-effort).
            }
        }
    }

    println!(
        "\n{} workspace codegen done — {}/{} package(s) succeeded",
        "[codegen]".green().bold(),
        succeeded,
        ws.packages.len(),
    );
    Ok(())
}

fn execute_single(
    config: &Config,
    package: &Path,
    mcp: bool,
    clean: bool,
    out_dir: Option<&Path>,
) -> Result<()> {
    let pkg_root = resolve_pkg_root(package)?;
    let rust_root = config.resolve_source_path(SourcePathKey::RustRoot)?;
    let interfaces_lib = config.resolve_source_path(SourcePathKey::InterfacesLib)?;
    let contracts_dir = config.resolve_source_path(SourcePathKey::Contracts)?;
    let interfaces_proto = config.resolve_source_path(SourcePathKey::InterfacesProto)?;
    let runtime_proto = config.resolve_source_path(SourcePathKey::RuntimeProto)?;
    let robonix_py = config.resolve_source_path(SourcePathKey::RobonixPy).ok();

    // Where to place proto_gen/ and robonix_mcp_types/. Defaults to package root;
    // override for packages that want them inside a sub-dir (e.g. tiago_bridge/).
    let out_root = match out_dir {
        Some(d) if d.is_absolute() => d.to_path_buf(),
        Some(d) => pkg_root.join(d),
        None => pkg_root.clone(),
    };
    let proto_gen = out_root.join("proto_gen");
    let mcp_types = out_root.join("robonix_mcp_types");
    let rbnx_build = pkg_root.join("rbnx-build");

    if clean {
        for p in [&proto_gen, &mcp_types, &rbnx_build] {
            if p.exists() {
                std::fs::remove_dir_all(p).ok();
            }
        }
    }

    let cargo_bin = if Path::new("/usr/bin/cargo").exists() {
        "/usr/bin/cargo".to_string()
    } else {
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
    };

    println!("{} package: {}", "[codegen]".bold(), pkg_root.display());
    println!(
        "{} robonix source: {}",
        "[codegen]".bold(),
        rust_root.display()
    );

    // 1. Regenerate robonix_proto/ (system-wide; safe to re-run).
    println!("{} robonix-codegen --lang proto ...", "[codegen]".bold());
    // TODO: support <pkg>/contracts and <pkg>/interfaces/lib union.
    run_cmd(
        "robonix-codegen proto",
        Command::new(&cargo_bin)
            .args(["run", "-p", "robonix-codegen", "--manifest-path"])
            .arg(rust_root.join("Cargo.toml"))
            .args(["--", "--lang", "proto", "-I"])
            .arg(&interfaces_lib)
            .arg("--contracts")
            .arg(&contracts_dir)
            .arg("-o")
            .arg(&interfaces_proto),
    )?;

    // 2. Optional: MCP dataclasses.
    if mcp {
        println!("{} robonix-codegen --lang mcp ...", "[codegen]".bold());
        std::fs::create_dir_all(&mcp_types).ok();
        run_cmd(
            "robonix-codegen mcp",
            Command::new(&cargo_bin)
                .args(["run", "-p", "robonix-codegen", "--manifest-path"])
                .arg(rust_root.join("Cargo.toml"))
                .args(["--", "--lang", "mcp", "-I"])
                .arg(&interfaces_lib)
                .arg("-o")
                .arg(&mcp_types),
        )?;
    }

    // 3. Package-local Python stubs via grpc_tools.protoc.
    println!(
        "{} grpc_tools.protoc → {}",
        "[codegen]".bold(),
        proto_gen.display()
    );
    std::fs::create_dir_all(&proto_gen)?;
    let proto_files: Vec<PathBuf> = std::fs::read_dir(&runtime_proto)?
        .chain(std::fs::read_dir(&interfaces_proto)?)
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("proto"))
        .collect();

    let mut protoc = Command::new("python3");
    protoc
        .args(["-m", "grpc_tools.protoc", "-I"])
        .arg(&runtime_proto)
        .arg("-I")
        .arg(&interfaces_proto)
        .arg(format!("--python_out={}", proto_gen.display()))
        .arg(format!("--grpc_python_out={}", proto_gen.display()));
    for f in &proto_files {
        protoc.arg(f);
    }
    let protoc_status = protoc
        .status()
        .with_context(|| "failed to execute python3 -m grpc_tools.protoc")?;
    if !protoc_status.success() {
        anyhow::bail!(
            "grpc_tools.protoc failed (exit {}).\n\
             Ensure grpcio-tools is installed in the active Python environment:\n\
             \n  pip install grpcio-tools\n\
             \nIf using conda, make sure the conda env is activated before running rbnx.",
            protoc_status.code().unwrap_or(-1)
        );
    }

    // 4. Write PYTHONPATH setup stub so `rbnx start` sees all the right paths.
    let ws_install = rbnx_build.join("ws").join("install");
    std::fs::create_dir_all(&ws_install)?;
    let mut py_parts: Vec<String> = vec![
        pkg_root.display().to_string(),
        proto_gen.display().to_string(),
    ];
    if mcp {
        py_parts.push(mcp_types.display().to_string());
    }
    if let Some(pp) = robonix_py {
        py_parts.push(pp.display().to_string());
    }
    let joined = py_parts.join(":");
    let setup_bash = ws_install.join("setup.bash");
    std::fs::write(
        &setup_bash,
        format!("#!/usr/bin/env bash\nexport PYTHONPATH=\"{joined}:${{PYTHONPATH:-}}\"\n"),
    )?;

    println!(
        "{} done — {}, {} setup.bash",
        "[codegen]".green().bold(),
        if mcp {
            "proto+mcp+stubs"
        } else {
            "proto+stubs"
        },
        if mcp_types.exists() {
            "mcp_types, "
        } else {
            ""
        }
    );
    Ok(())
}
