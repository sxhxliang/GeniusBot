import { useState } from "react";
import { api, downloadArtifact, type ArtifactRef, type TaskRecord } from "../api";
import { ErrorText, useAction } from "../ui";

export function ArtifactCards({ artifacts = [] }: { artifacts?: ArtifactRef[] }) {
  const action = useAction();
  if (!artifacts.length) return null;
  return <div className="artifact-list">
    {artifacts.map((file) => <div className="artifact-card" key={file.id}>
      <div className="min-w-0 flex-1"><strong className="block text-xs font-medium">{file.name}</strong><span className="font-mono text-[10px] text-muted">{file.size.toLocaleString()} bytes</span></div>
      <button className="btn small" disabled={action.busy} onClick={() => action.run(() => downloadArtifact(file))}>Download</button>
    </div>)}
    <ErrorText error={action.error} />
  </div>;
}

const statusLabels: Record<TaskRecord["status"], string> = {
  queued: "Queued", running: "Working", blocked: "Needs attention", completed: "Result submitted", failed: "Failed", cancelled: "Cancelled",
};

export function TaskCard({ task }: { task: TaskRecord }) {
  const action = useAction();
  const [note, setNote] = useState("");
  const [updated, setUpdated] = useState<TaskRecord>();
  const current = updated && updated.revision > task.revision ? updated : task;
  const active = ["queued", "running", "blocked"].includes(current.status);
  return <section className="task-card" aria-label="Delegated task">
    <div className="flex items-center gap-2"><span className={`tag ${current.status === "blocked" ? "warn" : current.status === "failed" ? "danger" : current.status === "completed" ? "ok" : "kind"}`}>{statusLabels[current.status]}</span><span className="text-[10px] text-muted">Delegated task</span></div>
    <div className="bubble-text">{current.summary}</div>
    {current.verification && <details><summary>Executor’s verification notes</summary><div className="bubble-text">{current.verification}</div></details>}
    <ArtifactCards artifacts={current.artifacts} />
    {current.status === "blocked" && <div className="task-resume">
      <textarea aria-label="Instructions to resume task" placeholder="Add information or instructions for the executor" value={note} onChange={(e) => setNote(e.target.value)} />
      <button className="btn small" disabled={action.busy} onClick={() => action.run(async () => { setUpdated(await api.resumeTask(current.id, note)); setNote(""); })}>Resume task</button>
    </div>}
    {active && <button className="btn small ghost" disabled={action.busy} onClick={() => action.run(async () => setUpdated(await api.cancelTask(current.id)))}>Cancel task</button>}
    <ErrorText error={action.error} />
  </section>;
}
