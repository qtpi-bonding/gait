await gait.wf.config({ name: "smoke-retry" });
await gait.wf.shell({
    id: "flaky",
    script: `if [ -f attempt.marker ]; then
  echo '{"ok": true}'
else
  touch attempt.marker
  echo 'not json'
fi`,
    outputSchema: {
        type: "object",
        required: ["ok"],
        properties: { ok: { type: "boolean" } },
    },
    retry: { times: 1, delay: 10 },
});
await gait.wf.shell({ id: "recovery", script: "echo RECOVERED" });