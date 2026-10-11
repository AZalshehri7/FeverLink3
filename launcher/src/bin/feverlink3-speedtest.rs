//! `feverlink3-speedtest.exe`: the Steam P2P speed test as a console program that needs only Steam.

fn main() {
    std::process::exit(feverlink3_launcher::speedtest::console::main_entry());
}
