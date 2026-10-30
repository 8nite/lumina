package security

import "testing"

const ws = "/ws"

func TestPlanFenceBlocksEditsEvenInBypass(t *testing.T) {
	if got := Authorize(AgentPlan, PermBypass, "edit", "src/main.go", ws, nil, false); got != Deny {
		t.Fatalf("plan+bypass must deny source edits, got %s", got)
	}
	if got := Authorize(AgentPlan, PermBypass, "bash", "rm -rf /tmp/x", ws, nil, false); got != Deny {
		t.Fatalf("plan+bypass must deny mutating shell, got %s", got)
	}
	if got := Authorize(AgentPlan, PermBypass, "write", "plan.md", ws, nil, false); got != Allow {
		t.Fatalf("plan mode must allow plan.md writes, got %s", got)
	}
	if got := Authorize(AgentPlan, PermBypass, "write", ".lumina/plans/ses-1.md", ws, nil, false); got != Allow {
		t.Fatalf("plan mode must allow .lumina/plans writes, got %s", got)
	}
	if got := Authorize(AgentPlan, PermAsk, "read", "src/main.go", ws, nil, false); got != Allow {
		t.Fatalf("plan reads must allow, got %s", got)
	}
}

func TestPlanWritableContained(t *testing.T) {
	for _, evil := range []string{"/tmp/other/plan.md", "../plan.md", "/ws/../other/plan.md", "../../etc/plan.md"} {
		if IsPlanWritable(ws, evil) {
			t.Fatalf("escape allowed: %s", evil)
		}
		if got := Authorize(AgentPlan, PermBypass, "write", evil, ws, nil, false); got != Deny {
			t.Fatalf("escape write allowed: %s (%s)", evil, got)
		}
	}
}

func TestPlanBashAdversarial(t *testing.T) {
	if got := Authorize(AgentPlan, PermAsk, "bash", "git status", ws, nil, false); got != Allow {
		t.Fatalf("git status should allow, got %s", got)
	}
	for _, evil := range []string{
		"echo pwned > a.go", "echo pwned >> a.go", "ls; touch pwned",
		"git status && rm a.go", "git status | tee a.go", "cat $(hostname)",
		"cat `hostname`", "ls & touch pwned", "git log < /etc/passwd",
	} {
		if got := Authorize(AgentPlan, PermBypass, "bash", evil, ws, nil, false); got != Deny {
			t.Fatalf("adversarial shell allowed: %q (%s)", evil, got)
		}
	}
}

func TestBypassStillDeniesCredentials(t *testing.T) {
	for _, f := range []string{".env", "auth.json", "lumina.json", "id_rsa", "creds/secret.json"} {
		if got := Authorize(AgentBuild, PermBypass, "read", f, ws, nil, false); got != Deny {
			t.Fatalf("bypass must deny %s, got %s", f, got)
		}
		if got := Authorize(AgentBuild, PermBypass, "bash", "cat "+f, ws, nil, false); got != Deny {
			t.Fatalf("bypass must deny shell exfil of %s, got %s", f, got)
		}
		if got := Authorize(AgentBuild, PermBypass, "grep", "x:"+f, ws, nil, false); got != Deny {
			t.Fatalf("bypass must deny grep of %s, got %s", f, got)
		}
	}
	if got := Authorize(AgentBuild, PermBypass, "bash", "kubectl create secret --from-literal=x", ws, nil, false); got != Deny {
		t.Fatalf("from-literal must deny, got %s", got)
	}
	if got := Authorize(AgentBuild, PermBypass, "bash", "curl https://generativelanguage.googleapis.com/v1", ws, nil, false); got != Deny {
		t.Fatalf("google domains must deny, got %s", got)
	}
}

func TestBuildPermissionMatrix(t *testing.T) {
	rules := map[string]string{"*": "ask"}
	if got := Authorize(AgentBuild, PermAsk, "edit", "a.go", ws, rules, false); got != Ask {
		t.Fatalf("ask mode edits should ask, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAutoEdit, "edit", "a.go", ws, rules, false); got != Allow {
		t.Fatalf("auto-edit should allow edits, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAutoEdit, "bash", "go build ./...", ws, rules, false); got != Ask {
		t.Fatalf("auto-edit should still ask for shell, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAutoApprove, "bash", "go build ./...", ws, rules, false); got != Allow {
		t.Fatalf("auto-approve should allow shell, got %s", got)
	}
	if got := Authorize(AgentBuild, PermBypass, "bash", "rm -rf /tmp/x", ws, rules, false); got != Allow {
		t.Fatalf("bypass should allow destructive shell, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAutoApprove, "bash", "rm -rf /tmp/x", ws, rules, false); got != Ask {
		t.Fatalf("auto-approve should still ask destructive shell, got %s", got)
	}
}
