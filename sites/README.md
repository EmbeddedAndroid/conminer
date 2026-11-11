# Node site configs

A copy of each node's `site.d/` (docs/operations.md, "Node-local config"):
its `fleet.toml` and other site files, the flash hook board files in
`flash/`, and any hook script that lives there. These are the files the
node's services load from `/etc/conminer/site.d`; this directory is not
mounted and `./cm build-id` does not hash it, so it records the config
without changing any node's build.

One directory per node, named after it. Backup copies (`*.bak*`) stay on
the node. After changing a node's `site.d`, refresh its copy here:

    ssh <node> 'cd ~/conminer-git/site.d && tar cf - --exclude="*.bak*" --exclude=README.md .' \
        | tar -C sites/<node> -xf -
