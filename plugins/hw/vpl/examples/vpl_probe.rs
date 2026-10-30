//! Prints the VPL probe report for this machine.

fn main() {
    match pf_vpl::probe() {
        Some(caps) => print!("{}", pf_vpl::probe::describe(caps)),
        None => println!(
            "no VPL hardware runtime (set ONEVPL_SEARCH_PATH to the directory of libmfx-gen.so.1.2, Gen12 and later only)"
        ),
    }
}
