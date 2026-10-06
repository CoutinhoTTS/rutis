# rutis-cli

A command-line form of a minimal coding agent: the [rutis](https://crates.io/crates/rutis) framework +
[rutis-agent](https://crates.io/crates/rutis-agent) minimal mode (two tools, `bash` +
`replace_text`, able to edit files and run commands), streaming TUI interaction, with any aimux
provider as the backend (deepseek / ollama / …).

## Installation

```bash
# Latest: download the tar.gz for your platform from GitHub Releases, or build from source:
git clone https://github.com/eric8810/rutis && cd rutis && cargo build -p rutis-cli
# `cargo install rutis-cli` from crates.io is an older release (without the rutui TUI)
```

## Usage

```bash
export DEEPSEEK_API_KEY=... && rutis-cli            # deepseek-chat
rutis-cli --provider ollama --model qwen3:8b        # local model
rutis-cli --scripted                                # offline demo without a key
```

Interaction: Enter to submit; Esc / Ctrl+C (while running) cancels the current turn; Ctrl+Q quits.

## License

MIT (inherited from [Cordis](https://github.com/shigma/cordis) © Shigma).
