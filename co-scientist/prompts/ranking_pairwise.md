You are an expert evaluator tasked with comparing two hypotheses.

Evaluate the two provided hypotheses (hypothesis 1 and hypothesis 2) and determine which one is superior based on the specified {{ idea_attributes | default('criteria') }}.

Provide a concise rationale for your selection, concluding with the phrase "better idea: <1 or 2>".

Goal: {{ goal }}

Evaluation criteria:
{{ preferences | default('') }}

Considerations:
{{ notes | default('') }}

Each hypothesis includes an independent review. These reviews may contain numerical scores. Disregard these scores in your comparative analysis, as they may not be directly comparable across reviews.

Hypothesis 1:
<HYPOTHESIS_TEXT id="{{ hypothesis_1_id }}">
{{ hypothesis_1 }}
</HYPOTHESIS_TEXT_END id="{{ hypothesis_1_id }}">

Hypothesis 2:
<HYPOTHESIS_TEXT id="{{ hypothesis_2_id }}">
{{ hypothesis_2 }}
</HYPOTHESIS_TEXT_END id="{{ hypothesis_2_id }}">

Review of hypothesis 1:
{{ review_1 }}

Review of hypothesis 2:
{{ review_2 }}

Reasoning and conclusion (end with "better idea: <1 or 2>"):

After your reasoning, you MUST emit exactly one marker to record the result. The marker is a single line on its own. It has three parts, in order:

1. A fixed prefix consisting of two opening square brackets, the literal text `MEMORY_OP`, and a colon. (The square brackets and colon are part of the marker syntax, not part of any tool name.)
2. The op name: exactly `record_tournament_match`.
3. A colon, then a JSON object payload, then two closing square brackets.

CRITICAL — common mistakes that cause dispatch failure:
- Do NOT use `memory_op` or `MEMORY_OP` as the op name. `MEMORY_OP` is part of the marker prefix above, not a tool name. The op name slot must be the actual tool name.
- Do NOT invent your own payload fields. The runtime tool expects exactly the schema below; extra fields like `match_id`, `loser`, `winner_elo_delta`, `traits_rewarded`, `trait_penalties` will be rejected.
- Do NOT use a string for `winner`. It must be the integer 1, 2, or 0.
- Do NOT wrap the marker in code fences, quotes, or any other delimiter.

REQUIRED payload fields (a JSON object with EXACTLY these four, no more, no less):
- `hypothesis_a` (integer): the FIRST hypothesis ID, which is {{ hypothesis_1_id }}
- `hypothesis_b` (integer): the SECOND hypothesis ID, which is {{ hypothesis_2_id }}
- `winner` (integer): `1` if hypothesis_a wins, `2` if hypothesis_b wins, `0` for a draw
- `rationale` (string): your reasoning explaining the decision

EXAMPLE — the JSON body your marker wraps, with concrete IDs from this turn. Replace the ID placeholders with the actual IDs from the prompt above. The marker delimiters themselves are real bracket characters in your output; this template shows only the body because the prompt-allowlist parser scans this file for literal marker prefixes.

  For hypothesis {{ hypothesis_1_id }} winning, the body is:
    {"hypothesis_a": {{ hypothesis_1_id }},"hypothesis_b": {{ hypothesis_2_id }},"winner": 1,"rationale": "Hypothesis {{ hypothesis_1_id }} has stronger mechanistic grounding."}

  For hypothesis {{ hypothesis_2_id }} winning, set `winner` to 2 and adjust the rationale.