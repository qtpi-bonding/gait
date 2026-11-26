await gait.wf.config({ name: "smoke-parallel" });
await Promise.all([
    gait.wf.shell({ id: "branch-a", script: "echo BRANCH_A" }),
    gait.wf.shell({ id: "branch-b", script: "echo BRANCH_B" }),
]);
await gait.wf.shell({ id: "join", script: "echo DONE" });