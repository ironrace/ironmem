# Security

- Never commit secrets, API keys, or credentials.
- Every `unsafe` Rust block must have an adjacent `// SAFETY:` comment explaining why it is sound.
- Do not expose raw internal errors to external callers when a safer user-facing error is appropriate.
