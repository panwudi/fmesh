use std::io::{Read, Write};
fn main() {
    let path=std::env::args().nth(1).expect("notice output path");
    let mut payload=String::new();
    std::io::stdin().read_to_string(&mut payload).unwrap();
    let mut file=std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    file.write_all(payload.as_bytes()).unwrap();
}
