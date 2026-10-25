//! Prints the line of `tools/addr/out/verified-builds.txt` for one build: its name, PE timestamp and the hash of its row.
//!
//! `cargo run --release -p feverlink3_native --example verified_line -- <TransportFever3.exe>` runs the module's own search on
//! the executable (read as data, never run); `-- <port report .txt>` takes the row of a committed `port_build.py` report. Add
//! the line only for a build that was played and checked.

use feverlink3_native::{build_table, search};

fn main() {
    let path = std::env::args().nth(1).expect("usage: verified_line <TransportFever3.exe | port report .txt>");
    let row = if path.ends_with(".txt") {
        build_table::parse_report(&std::fs::read_to_string(&path).expect("read the report"))
            .ok_or("not a complete report")
    } else {
        let img = search::Image::from_file(&std::fs::read(&path).expect("read the executable")).expect("a PE32+ image");
        let proof = search::Proof::parse(search::PROOF).expect("the embedded proof");
        search::search(&proof, &img, None).map(|(b, _)| b).map_err(|_| "the search did not prove every site")
    };
    match row {
        Ok(b) => println!("{} {:#010x} {}", b.name, b.timestamp, build_table::row_hash(&b)),
        Err(why) => {
            eprintln!("{path}: {why}");
            std::process::exit(1);
        }
    }
}
