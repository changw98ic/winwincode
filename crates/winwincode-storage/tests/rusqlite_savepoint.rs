use rusqlite::Connection;

#[test]
fn savepoint_names_are_identifiers_not_sql() -> rusqlite::Result<()> {
    assert_eq!(rusqlite::version(), "3.51.3");
    let mut connection = Connection::open_in_memory()?;
    let mut transaction = connection.transaction()?;
    let savepoint =
        transaction.savepoint_with_name("safe_name\"; CREATE TABLE injected(value); --")?;
    savepoint.commit()?;

    let injected: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'injected'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(injected, 0);
    Ok(())
}
