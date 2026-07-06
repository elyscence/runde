use std::fs;
use std::path::Path;

// fix ts later

fn main() {
    let env_path = Path::new(".env");
    println!("cargo:rerun-if-changed=.env");

    let mut relay_url = String::new();

    if env_path.exists() {
        let content = fs::read_to_string(env_path).expect("не удалось прочитать .env");
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if key == "CUSTOM_RELAY_URL" {
                    relay_url = value.to_string();
                }
            }
        }
    }

    println!("cargo:rustc-env=CUSTOM_RELAY_URL={relay_url}");
}
