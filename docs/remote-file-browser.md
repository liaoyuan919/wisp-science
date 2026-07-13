# Remote file browser

The Files panel can switch between the current local project and registered
SSH execution contexts. Selecting an SSH context opens the remote user's home
directory and supports:

- entering an absolute path (or `~` / `~/...`) and pressing Enter;
- moving to the parent directory;
- opening child directories;
- viewing non-hidden file names and sizes.

Remote browsing uses the existing `ssh:<alias>` `ExecutionContext` connection
snapshot and the system OpenSSH client. It honors the configured SSH alias,
user, port, identity-file path, SSH config, and agent. No private-key contents
are stored in SQLite or copied by the browser.

This first version is read-only. Remote file preview, search, download, upload,
rename, and deletion are intentionally out of scope. Large remote data remains
in place rather than being synchronized to the local project.

## Manual smoke test

1. Register or import an SSH host and confirm its `ssh:<alias>` context appears
   in the Contexts panel.
2. Open Files and select the SSH host in **File location**.
3. Confirm the remote home directory loads, then open a child directory, use
   the parent button, and enter an absolute path.
4. Disconnect the host or enter an inaccessible path and confirm Files shows a
   retryable error without blocking the rest of the app.

Automated tests use a fake remote-directory runner and a mocked Tauri command;
they never require a real SSH host or network access.
