Your task is the issue `{{ issue.id }}`, in `{{ rig.path }}`:

<task>
{{ issue.description }}
</task>

Work through these steps.

1. Read every Markdown file under `{{ rig.path }}/docs` to its end, one file after another: do not skim, search or
   sample them, the task needs all of their text.

2. Write `{{ rig.path }}/INDEX.md` as the task asks. Change nothing else.

3. Finish. When the file is written: `loop-manager finish pass --summary "<what INDEX.md holds>"`.
   If you cannot write it: `loop-manager finish fail --summary "<why>"`.
