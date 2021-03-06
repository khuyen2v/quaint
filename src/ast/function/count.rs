use crate::ast::DatabaseValue;

#[derive(Debug, Clone, PartialEq)]
/// Returns the number of rows that matches a specified criteria.
pub struct Count<'a> {
    pub(crate) exprs: Vec<DatabaseValue<'a>>,
}

/// Count of the underlying table where the given expression is not null.
///
/// ```rust
/// # use quaint::{ast::*, visitor::{Visitor, Sqlite}};
/// let query = Select::from_table("users").value(count(asterisk()));
/// let (sql, _) = Sqlite::build(query);
/// assert_eq!("SELECT COUNT(*) FROM `users`", sql);
/// ```
#[inline]
pub fn count<'a, T>(expr: T) -> Count<'a>
where
    T: Into<DatabaseValue<'a>>,
{
    Count {
        exprs: vec![expr.into()],
    }
}
