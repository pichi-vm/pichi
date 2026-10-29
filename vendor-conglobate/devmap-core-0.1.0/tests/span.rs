// SPDX-License-Identifier: Apache-2.0

use devmap_core::Span;

#[test]
fn span_preserves_its_start_and_length() {
    let span = Span::<u64>::new(17, 23);

    assert_eq!(span.start, 17);
    assert_eq!(span.length, 23);
}
