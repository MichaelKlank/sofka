# Changelog

## v0.31.14 - 2026-10-20

This release fixes owner navigation in Shift-J and `:bundle`. A pod owned by a custom resource now opens the correct kind.

### Fixes

- Shift-J and `:bundle` now find the owner of a pod in the correct API group. Before this fix, a custom resource with the same kind name as a built-in kind opened the wrong kind. If the cluster does not serve the owner group, sofka reports the owner as unresolved.
