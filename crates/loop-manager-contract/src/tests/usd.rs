use crate::usd::Usd;

#[test]
fn an_amount_prints_as_its_exact_decimal_without_trailing_zeros() {
    let print = |pico| Usd(pico).to_string();
    assert_eq!(print(0), "0");
    assert_eq!(print(5_000_000_000_000), "5");
    assert_eq!(print(1_432_440_000), "0.00143244");
    assert_eq!(print(1), "0.000000000001");
    assert_eq!(print(-500_000_000_000), "-0.5");
}

#[test]
fn a_harness_float_rounds_to_the_nearest_pico_usd() {
    assert_eq!(Usd::from_f64(0.00143244), Usd(1_432_440_000));
    assert_eq!(Usd::from_f64(0.0), Usd(0));
    assert_eq!(Usd::from_f64(f64::INFINITY), Usd(i64::MAX));
}
