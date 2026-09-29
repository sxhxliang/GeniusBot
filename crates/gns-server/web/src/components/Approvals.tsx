// Pending tool approvals (ShellGuardPolicy & co).

import { useState } from "react";
import { api, type ApprovalRequest } from "../api";
import { ErrorText, JsonView, fmtTime, useAction } from "../ui";

export function ApprovalCard({ request }: { request: ApprovalRequest }) {
  const [reason, setReason] = useState("");
  const action = useAction();
  const decide = (approved: boolean) => action.run(() => api.decide(request.id, approved, reason || undefined));
  return (
    <div className="approval">
      <div className="approval-head">
        <b>Approval needed</b> · {request.agent_name} wants to run <code>{request.tool}</code>
        <span className="muted small"> · expires {fmtTime(request.expires_at)}</span>
      </div>
      <div className="muted small">
        {request.reason} ({request.policy})
      </div>
      <JsonView value={request.args} maxHeight={200} />
      <div className="flex flex-wrap items-center gap-2">
        <input className="basis-full sm:flex-1 sm:basis-auto" aria-label="Approval reason" value={reason} placeholder="Reason (optional)" onChange={(e) => setReason(e.target.value)} />
        <button className="btn primary small" disabled={action.busy} onClick={() => decide(true)}>
          Approve
        </button>
        <button className="btn danger small" disabled={action.busy} onClick={() => decide(false)}>
          Deny
        </button>
      </div>
      <ErrorText error={action.error} />
    </div>
  );
}

export function ApprovalsPanel({ approvals }: { approvals: ApprovalRequest[] }) {
  return (
    <div className="mx-auto flex w-full max-w-4xl flex-col gap-4">
      <div className="mb-2"><h2>Approvals</h2><p className="mt-1 text-xs text-muted">Review tool requests before your agents proceed.</p></div>
      {approvals.length === 0 && <div className="rounded-xl border border-dashed border-line px-6 py-14 text-center"><h3>All clear</h3><p className="text-sm text-muted">No pending approvals. Requests that need your review will appear here.</p></div>}
      {approvals.map((a) => (
        <ApprovalCard key={a.id} request={a} />
      ))}
    </div>
  );
}
