# Security

Ferry copies and deletes files on your servers and installs its own updates, so a security bug in it matters. Please report one privately.

## Reporting a vulnerability

Use GitHub's private reporting: on the repository's **Security** tab, choose **Report a vulnerability**. Please don't open a public issue for it.

Say what an attacker can do and what they need for it (on the same network, on the laptop, control of a server, and so on), and how to reproduce it. You'll get an answer within a week, and a fix or a plan once the report is confirmed. Reporters are credited in the release notes unless they'd rather not be.

## Supported versions

Only the latest release gets fixes. Installed copies update themselves, so a fix reaches them once it is released.

## Most worth looking at

- **Updates.** An installed Ferry runs a downloaded installer silently. It only does so for a release whose `SHA256SUMS.txt` carries Ferry's update signature, over https, from GitHub ([How updates work](docs/updates.md)).
- **Deletes at the destination.** Ferry mirrors deletions, guarded by the rules in [safety.md](docs/safety.md).
- **Networks.** Ferry decides when it may copy by recognising a network and checking the path to the server ([What Ferry trusts](docs/how-it-works.md#what-ferry-trusts)). That it trusts a server answering on a recognised network is a known limit, on the [roadmap](docs/roadmap.md).
- **The local pipe** between Ferry's windows and its engine, which carries SMB passwords on their way to Windows Credential Manager.
- **Settings backups,** which are restored from a file the user picks.
