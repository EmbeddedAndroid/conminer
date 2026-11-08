# Console login credentials

A console sitting at a login prompt is classified `login_wait`: the board is
up but not commandable. With no credentials configured, `run_command` refuses
there with `LOGIN_REQUIRED` and types nothing but its newline probe. With a
login configured for that console, `run_command` logs in by itself and then
runs the command, in the same transaction.

## The file

The config names the file; it is unset by default. Which file, and whether a
node has one at all, is that node's choice, so it goes in a site file
(`site.d/*.toml`, see docs/operations.md) rather than the shared
`conminer.toml`:

```toml
[credentials]
file = "/var/lib/conminer/credentials.toml"
```

The file is TOML, one table per console, keyed the same way as
`[devices."<selector>"]` in `conminer.toml`: the console's port path or the
label an operator gave it.

```toml
[devices."/dev/serial/by-id/usb-FTDI_Example-if01-port0"]
user = "root"
password = "..."

[devices."lab-board-ap"]
user = "admin"
# password may be omitted for an account without one
```

Unknown keys are refused, so a typo cannot silently drop a password.

The file is read when a console is found at a credential gate, on every such
occasion, so editing it takes effect without a restart. It is refused, and
the console treated as having no login, unless all of these hold:

- it is a regular file (a symlink is followed to the file it names);
- its mode is exactly 0600;
- it is owned by the uid the conminer process runs as (in the stock container
  image, root).

A refusal is reported in the `LOGIN_REQUIRED` error's
`detail.transaction.detail.credentials.refused` with the reason (mode, owner,
or the line a parse error is on). The reason never quotes the file.

### Where to put it

In the compose deployment the `conminer-data` volume (`/var/lib/conminer`) is
writable by mcpd and survives image rebuilds, so the file can live there
without a compose change. Create it from the host without the password
passing through a command line or a shell history:

    docker exec -i conminer-mcpd sh -c \
      'umask 077; cat > /var/lib/conminer/credentials.toml' < local-file.toml
    docker exec conminer-mcpd sh -c \
      'chown 0:0 /var/lib/conminer/credentials.toml; \
       chmod 600 /var/lib/conminer/credentials.toml'

A node whose `conminer.toml` names a file it does not have simply has no
logins: a missing file is a refusal, reported at the gate, never a startup
failure.

## The login

At a credential gate `run_command` answers the gate the console is sitting
at: a user gate (`login:`, `Username:`) gets the user name, typed with echo
verification; a password gate (`Password:`, U-Boot's `Enter password`) gets the
password, paced but never echo-verified, because a sane console does not echo
it. It then waits, within the call's `timeout_s`, for a shell prompt.

A gate that comes back after it was answered means the login was rejected.
That ends the transaction with `LOGIN_REQUIRED` and
`detail.login.result: "rejected"`; the password is never typed in a loop.

The transaction records what was sent:

```json
"login": {"user": "root", "sent": ["root", "<credential>"], "result": "ok"}
```

`result` is `ok`, `rejected` or `no_prompt`. The same record is stored with the
`run_command` event.

## Where the password never goes

- not in the registry or any device store, and so not in `export_session`;
- not in any tool response: the transaction's command, output, preamble,
  rungs and detail are all redacted before they leave the runner, so even a
  broken console that echoes the password returns `<credential>`;
- not in the logs: the password is held in a type whose `Debug` prints the
  marker and which cannot be serialized.

The raw RX stream is the board's own bytes and is stored verbatim (spec
§8.5); a console that echoes its password puts it there, and only fixing the
console changes that.

Tools that read or write host files (`ingest_file`, `ingest_pstore`,
`push_file`, `transfer_file`, `export_session`) refuse the credentials file
with `PERMISSION_DENIED`, whatever name or link reaches it, so it cannot be
mined into a store, pushed to a board, or overwritten.

## Tests

Suite `credentials` drives `run_command` through the MCP handler against a
scripted console on a TCP socket that runs a real login state machine, and
searches every sink after each scenario: every file under the data
directory, an exported session, every tool response, and everything logged
at TRACE.
