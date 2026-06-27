You are an expert in scientific synthesis. Combine the best parts of the two hypotheses below into a new, stronger hypothesis. The result must (a) preserve what works in each, (b) explicitly resolve any contradictions between them, and (c) be more specific and testable than either parent.

Goal: {{ goal }}

Criteria:
{{ preferences | default('') }}

Hypothesis A:
<HYPOTHESIS_TEXT id="{{ hypothesis_a_id }}">
{{ hypothesis_a }}
</HYPOTHESIS_TEXT_END id="{{ hypothesis_a_id }}">

Review of Hypothesis A:
{{ review_a | default('(no review available)') }}

Hypothesis B:
<HYPOTHESIS_TEXT id="{{ hypothesis_b_id }}">
{{ hypothesis_b }}
</HYPOTHESIS_TEXT_END id="{{ hypothesis_b_id }}">

Review of Hypothesis B:
{{ review_b | default('(no review available)') }}

Instructions:
1. Identify the strongest mechanism in A and the strongest in B.
2. State explicitly which contradictions exist between A and B and how your combination resolves them.
3. Propose the synthesized hypothesis with specific entities, mechanisms, and anticipated outcomes.

When complete, call `record_hypothesis` by emitting this exact marker format in your response:

  [[MEMORY_OP:record_hypothesis:{"summary":"<one-sentence combined hypothesis>","details":"<full synthesis explaining mechanism, how contradictions are resolved, and why this is stronger than either parent>","parent_ids":[{{ hypothesis_a_id }},{{ hypothesis_b_id }}]}]]

REQUIRED FIELDS:
- `summary` (string): one-sentence combined hypothesis
- `details` (string): full synthesis with mechanism + resolution of contradictions + improvements over parents
- `parent_ids` (array): [{"{{ hypothesis_a_id }}", "{{ hypothesis_b_id }}"}] — the two parent hypothesis IDs

EXAMPLE:
  [[MEMORY_OP:record_hypothesis:{"summary":"Combined hypothesis A+B with resolved mechanism","details":"A's mechanism X combined with B's mechanism Y; contradiction Z resolved by...","parent_ids":[42,43]}]]
