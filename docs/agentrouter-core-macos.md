# AgentRouter macOS Core component

`agentrouter-core-macos.yml` builds Codex Core as an immutable, architecture-specific
Runtime v2 component. It does not publish or change an AgentRouter Runtime channel.

The workflow runs for both Apple Silicon and Intel when the Core release contract is
changed on `main`, when an `agentrouter-core-v*` tag is pushed, or when it is manually
dispatched. Default-branch and tag runs require Developer ID signing and Apple
notarization. Manual runs may use ad-hoc signing for an internal build probe.

Each artifact contains only:

- `codex`
- `agentrouter-core.json`

The manifest records the source and upstream Git SHAs, binary SHA-256, signature
authority, supported Shell versions, and the `CODEX_CLI_PATH` activation contract.
Artifacts are retained for seven days as candidates. Promotion to the AgentRouter Dev
or Release channel is a separate, explicitly authorized operation after composed-app
acceptance.

Repository Actions secrets required for publishable runs are
`APPLE_CERTIFICATE_BASE64`, `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_API_KEY_P8_BASE64`, `APPLE_API_KEY_ID`, and `APPLE_API_ISSUER_ID`. The
workflow derives the exact signing identity from the imported temporary keychain.
