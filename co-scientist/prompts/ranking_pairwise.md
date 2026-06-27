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

After your reasoning, call the `record_tournament_match` tool by emitting this exact marker format in your response:

  [[MEMORY_OP:record_tournament_match:{"hypothesis_a":{{ hypothesis_1_id }},"hypothesis_b":{{ hypothesis_2_id }},"winner":<1 or 2>,"rationale":"<your reasoning>"}}]]

REQUIRED FIELDS:
- `hypothesis_a` (integer): first hypothesis ID ({{ hypothesis_1_id }})
- `hypothesis_b` (integer): second hypothesis ID ({{ hypothesis_2_id }})
- `winner` (integer): 1 = hypothesis_a wins, 2 = hypothesis_b wins, 0 = draw
- `rationale` (string): your reasoning explaining the decision

EXAMPLE:
  [[MEMORY_OP:record_tournament_match:{"hypothesis_a":42,"hypothesis_b":43,"winner":1,"rationale":"Hypothesis 42 has stronger mechanistic grounding and clearer testability criteria."}]]
