# E63 · prompt_file — the project's system block

Scripted (`costs: free`): the model is the parity chat stub, which records each
request's system message and answers plain text.

**PASS** when every check holds:

| check | what |
|---|---|
| seeded | a fresh project's `.fantastic/prompt.md` is `{tree}\n\n{self}\n\n{howto}` |
| turn1 · turn1_default | the first turn's system block is the default one (its id, the how-tos), placeholders filled |
| ai_names_file | the canvas AI's record names `file_bridge_id: prompt_file` and `system_prompt_file: .fantastic/prompt.md` |
| records_persist | the kernel's records never persist through the prompt bridge (it is rooted at the project, not `.fantastic`) |
| turn2 · turn2_edited | after an edit of the file, the next turn's block is the edit — its own id still filled, the replaced default gone — with no restart |
