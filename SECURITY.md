# Security

BigSearch reads every file in the folders it indexes, parses documents it did
not create and keeps a record of them on disk, so parser bugs, path handling
and the socket are all security-relevant.

## Reporting a vulnerability

Please report privately through GitHub: **Security → Report a vulnerability**
on this repository. Do not open a public issue for a vulnerability.

Include the commit or version, what an attacker controls (a file in an indexed
folder, a connection to the socket, …), the impact and a minimal reproducer.
Use test files you created yourself.

## Supported versions

Until the first stable release, fixes land on `main` only.
