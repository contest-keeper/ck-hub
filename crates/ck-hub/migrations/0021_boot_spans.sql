-- When each boot of a stick was first and last heard from. Two boots of one stick that are heard
-- from at the same time mean the stick's credential is in use on two machines: a normal reboot
-- ends the old boot before the new one starts. Rows from before this migration have no times and
-- are left out of that check.
ALTER TABLE heartbeat_boots ADD COLUMN first_seen timestamptz;
ALTER TABLE heartbeat_boots ADD COLUMN last_seen timestamptz;
