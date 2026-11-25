//! Checks the secret store without showing secrets:
//!   cargo run --release -p rdp-session --example secret_probe -- key=value [key=value ...]
//! Prints whether a secret with exactly these attributes exists, and its length.
//! `--store <value>` stores a value under the attributes instead, `--clear` removes it (tests).
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let store = args
        .iter()
        .position(|a| a == "--store")
        .and_then(|i| args.get(i + 1).cloned());
    let pairs: Vec<(String, String)> = args
        .iter()
        .filter_map(|a| {
            a.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let attrs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    if args.iter().any(|a| a == "--clear") {
        rdp_session::secret::clear(&attrs);
        println!("gelöscht");
        return;
    }
    if let Some(v) = store {
        println!("{:?}", rdp_session::secret::store(&attrs, "test", &v));
        return;
    }
    match rdp_session::secret::lookup(&attrs) {
        Some(v) => println!("gefunden, {} Zeichen", v.len()),
        None => println!("nicht gefunden"),
    }
}
