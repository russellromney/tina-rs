021 closeout:

- preferred public surface is now the surface exercised by docs, examples, and
  downstream consumer proofs
- common path reads materially closer to Tokio while keeping explicit
  message/effect semantics
- public examples and README now explain the story before showing the code
- self-address and self-loop ergonomics are pinned to `ctx.me()` and
  `ctx.send_self(...)`
- high-signal user-surface tests now use the preferred vocabulary
- workspace verification is green

Verdict: phase goal met; remaining syntax ideas belong to optional follow-on
cleanup, not to the core 021 bar.
