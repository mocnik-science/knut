//! Prefix/suffix list diff used for way node lists and relation member lists.

use crate::{Error, Result};

/// Diff of `new` against `old`: `new = old[..prefix] ++ ins ++ old[prefix+del..]`.
pub fn diff<'a, T: PartialEq>(old: &[T], new: &'a [T]) -> (usize, usize, &'a [T]) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    (prefix, old.len() - prefix - suffix, &new[prefix..new.len() - suffix])
}

/// Applies a diff produced by [`diff`].
pub fn apply<T: Clone>(old: &[T], prefix: usize, del: usize, ins: Vec<T>) -> Result<Vec<T>> {
    let cut = prefix.checked_add(del).ok_or(Error::Corrupt("diff overflow".into()))?;
    if cut > old.len() {
        return Err(Error::Corrupt("diff out of range".into()));
    }
    let mut out = Vec::with_capacity(prefix + ins.len() + old.len() - cut);
    out.extend_from_slice(&old[..prefix]);
    out.extend(ins);
    out.extend_from_slice(&old[cut..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn diff_apply_roundtrip(old in prop::collection::vec(0u8..4, 0..30),
                                new in prop::collection::vec(0u8..4, 0..30)) {
            let (p, d, ins) = diff(&old, &new);
            prop_assert_eq!(apply(&old, p, d, ins.to_vec()).unwrap(), new);
        }
    }
}
