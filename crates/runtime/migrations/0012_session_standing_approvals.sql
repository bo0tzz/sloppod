-- Standing approvals now cover one session each; ones that covered all of an owner's sessions stop matching.
update standing_approvals set revoked_at = now() where session_id is null and revoked_at is null;
