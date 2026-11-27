-- When Forge makes a stick with first-boot enrolment, it picks the stick id upfront and sends it
-- with the code request. The stick row is created right away with an empty secret (shown as not
-- activated) until the stick claims its own secret at `/api/v1/enrol/claim`.
ALTER TABLE enrolment_codes ADD COLUMN stick_id text;
