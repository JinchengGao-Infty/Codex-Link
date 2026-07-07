You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue
- What was already tried and failed, so the next LLM does not repeat it
- What was already verified as working, so the next LLM does not redo or re-verify it

Preserve exact identifiers verbatim instead of paraphrasing them: file paths, symbol and function names, commands, error messages, URLs, and IDs. They are search anchors for the next LLM; a paraphrased path or error message cannot be grepped.

Keep source and time attribution explicit. If the user pasted a log, transcript, or another agent's output, label it as user-provided evidence and do not rewrite it as work performed by the current assistant. Treat assistant messages before this compaction as historical pre-compaction work: future-tense statements such as "I will", "next", or "I am going to" are not automatically current post-compaction instructions. If the latest user request is still in progress, include an explicit in-flight status: completed work, current step, next step, and unresolved blockers.

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.
