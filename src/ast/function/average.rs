use crate::ast::Column;

#[derive(Debug, Clone, PartialEq)]
pub struct Average<'a> {
    pub(crate) column: Column<'a>,
}

/// Calculates the average value of a numeric column.
///
/// ```rust
/// # use quaint::{ast::*, visitor::{Visitor, Sqlite}};
/// let query = Select::from_table("users").value(avg("age"));
/// let (sql, _) = Sqlite::build(query);
/// assert_eq!("SELECT AVG(`age`) FROM `users`", sql);
/// ```
#[inline]
pub fn avg<'a, C>(col: C) -> Average<'a>
where
    C: Into<Column<'a>>,
{
    Average { column: col.into() }
}
