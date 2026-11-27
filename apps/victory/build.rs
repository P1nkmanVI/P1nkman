//! P1nkman: embeds the program's icon (`p1nkman.rc`, `p1nkman.ico`) with the
//! Windows SDK's resource compiler. `rc.exe` is on the PATH only in a Visual
//! Studio developer prompt, so it is also looked for in the Windows SDK's own
//! folders; without it the build goes on, without the icon, and says so.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=p1nkman.rc");
    println!("cargo:rerun-if-changed=p1nkman.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("p1nkman.res");
    for rc in candidates() {
        let ran = Command::new(&rc)
            .arg("/nologo")
            .arg(format!("/fo{}", output.display()))
            .arg("p1nkman.rc")
            .status();
        match ran {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bin=victory={}", output.display());
                return;
            }
            Ok(status) => {
                println!("cargo:warning=rc.exe ({}) failed ({status}): building without the P1nkman icon", rc.display());
                return;
            }
            Err(_) => continue,
        }
    }
    println!(
        "cargo:warning=the Windows SDK resource compiler (rc.exe) was not found: building without the P1nkman icon \
         (install a Windows 10/11 SDK with the Visual Studio Build Tools to have it)"
    );
}

/// `rc.exe` on the PATH, then the newest of each Windows SDK's `bin\<version>\<arch>\rc.exe`.
fn candidates() -> Vec<PathBuf> {
    let mut found = vec![PathBuf::from("rc.exe")];
    let arch = if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") { "arm64" } else { "x64" };
    let roots = [env::var_os("ProgramFiles(x86)"), env::var_os("ProgramFiles")];
    for root in roots.into_iter().flatten() {
        let bin = PathBuf::from(root).join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else { continue };
        let mut versions: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("10.")))
            .collect();
        // Newest first: "10.0.26100.0" sorts after "10.0.19041.0" by its numbers.
        versions.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.split('.').map(|part| part.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>())
                .unwrap_or_default()
        });
        for version in versions.into_iter().rev() {
            let rc = version.join(arch).join("rc.exe");
            if rc.is_file() {
                found.push(rc);
            }
        }
        // (an older SDK layout: bin\<arch>\rc.exe)
        let rc = bin.join(arch).join("rc.exe");
        if rc.is_file() {
            found.push(rc);
        }
    }
    found
}
