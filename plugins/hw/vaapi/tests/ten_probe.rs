/// The probe ends with an answer on any device, twice the same.
#[test]
fn the_ten_bit_shrink_probe_answers() {
    let first = pf_vaapi::shrinks_ten();
    eprintln!("shrinks_ten={first}");
    assert_eq!(pf_vaapi::shrinks_ten(), first);
}
