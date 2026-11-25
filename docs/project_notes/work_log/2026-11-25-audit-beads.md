# 2026-11-25 — The v0.23.0 source audit is filed as 30 beads

An outside audit of acs v0.23.0 claimed about 55 findings. Each top finding and most of the low ones were checked
against the source before filing; the ones that held, or held in a narrower form, became 30 beads (2 P1, 6 P2, 16 P3,
6 P4). Claims that did not hold were left out: the cp "abandoned file resumes as success" HIGH (the receiver counts the
failure locally, and an upload's receiver is fresh per dial), the knock BindInterface errno misread (the error is
wrapped, so it is reported rather than treated as unreachable), and update_check's take_warning ordering (needs a
corrupt state file). Where the audit's proposed fix was wrong, the bead says so: no byte cap on output escape strings,
since graphics payloads are megabytes; the redial deadline is restarted on hand-off rather than never enforced.
