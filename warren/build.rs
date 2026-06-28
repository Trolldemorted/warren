use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=migrations_atlas");
    let migrations_dir = Path::new("migrations_atlas");

    let mut entries: Vec<String> = match fs::read_dir(migrations_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort();

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let dest = Path::new(&out_dir).join("migrations.rs");
    let mut s = String::from("pub static MIGRATIONS: &[(&str, &str, &str)] = &[\n");
    for name in &entries {
        let path = migrations_dir.join(name);
        let content = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {}: {e}", path.display());
        });
        let version = name.split('_').next().unwrap_or(name.as_str()).to_string();
        let description = name.trim_end_matches(".sql").to_string();
        let escaped = content
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        s.push_str(&format!(
            "(\"{version}\", \"{description}\", \"{escaped}\"),\n"
        ));
    }
    s.push_str("];\n");
    fs::write(&dest, s).expect("write migrations.rs");
}
