// Embeds every rules/*.yml as a built-in rule set named after its file.
use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=rules");
    let dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("rules");
    let mut sets: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("rules/")
        .filter_map(|e| {
            let path = e.ok()?.path();
            (path.extension()? == "yml").then(|| {
                let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
                (name, path.to_str().unwrap().to_owned())
            })
        })
        .collect();
    sets.sort();
    let mut out = String::from("pub(crate) static BUILTIN: &[(&str, &[u8])] = &[\n");
    for (name, path) in &sets {
        println!("cargo:rerun-if-changed={path}");
        out += &format!("    ({name:?}, include_bytes!({path:?})),\n");
    }
    out += "];\n";
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("builtin.rs"), out).unwrap();
}
