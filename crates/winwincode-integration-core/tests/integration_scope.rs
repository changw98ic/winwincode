// SPDX-License-Identifier: Apache-2.0

use winwincode_integration_core::IntegrationScope;

#[test]
fn integration_scope_is_bounded_and_portable() {
    let scope = IntegrationScope::try_new("repository:acme/widget").expect("portable scope");
    assert_eq!(scope.as_str(), "repository:acme/widget");
    assert!(IntegrationScope::try_new("").is_err());
    assert!(IntegrationScope::try_new("contains whitespace").is_err());
    assert!(IntegrationScope::try_new("x".repeat(513)).is_err());
}
