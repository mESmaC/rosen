## 6. Out of scope (v1)

- The Rosen-native remote storage backend (designed for behind the backend interface, built later).
- A hosted server, an internet-facing web login (OAuth), and regional cache tiers.
  The network listener and join-key enrollment are in v1 (R14).
  The hosted platform's web login will mint the same join keys.
- Clustered servers and the 1000-agent target.
- First-class Linux and macOS support (best-effort only).
- Running checks or builds inside Rosen.
- A virtual filesystem (FUSE or ProjFS) for workspaces.
- Live two-way sync with a git remote.
  The mirror is push-only.
- Object garbage collection, and redaction or purge of objects.
- Per-directory access control.
- A GUI.
  Velocity is the UI.
- AIRC's agent tools and its view.
  These stay in Velocity; the channel's state is on the server (R13).

---
