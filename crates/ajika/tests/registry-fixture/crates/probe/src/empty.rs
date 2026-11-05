use ajika::__private::Registration;
fn main() {
    // A Cargo dependency that the binary does not reference need not be linked.
    assert_eq!(
        ajika::__private::inventory::iter::<Registration>
            .into_iter()
            .count(),
        0
    );
}
