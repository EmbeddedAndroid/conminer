# Node-local config

Every `*.toml` file here is layered over `conminer.toml`, in name order, by
every service. Put what belongs to this node in them: its consoles' hooks and
`[devices]` sections, its credentials path, anything naming its own hardware.
`conminer.toml` stays the compiled defaults and the same on every node.

Nothing here but this README is tracked, `cm sync` never overwrites the
directory, and `./cm build-id` does not hash it. See docs/operations.md.
