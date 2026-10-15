//! Synthetic acceptance fixture only. Destination is public; a fresh synthetic
//! master key arrives through stdin and is never echoed or persisted as text.
#[path = "../tests/support/legacy_fixture.rs"]
mod fixture;
use std::io::Read;
fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("new synthetic destination argument");
    let mut input = zeroize::Zeroizing::new(String::new());
    std::io::stdin()
        .take(66)
        .read_to_string(&mut input)
        .unwrap();
    let value = input.trim();
    assert!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "synthetic key input must be hex"
    );
    let mut master = zeroize::Zeroizing::new([0u8; 32]);
    for (i, v) in master.iter_mut().enumerate() {
        *v = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap();
    }
    fixture::create(std::path::Path::new(&path), &master);
    if let Some(version) = std::env::args().nth(2) {
        let version: u32 = version.parse().expect("public synthetic schema number");
        fixture::connection(&std::path::Path::new(&path).join("smith.db"), &master)
            .pragma_update(None, "user_version", version)
            .unwrap();
    }
    println!("Synthetic encrypted schema25 fixture created.");
}
