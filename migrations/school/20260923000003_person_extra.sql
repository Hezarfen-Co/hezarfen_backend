-- Profile extras on app_user: gender, postal address, and an emergency
-- contact (name + phone). All NULL until filled in; every column rides the
-- three-state PATCH (omitted keeps, "" clears, a value validates), exactly
-- like phone and birth_date before them.
ALTER TABLE app_user
    ADD COLUMN gender                   TEXT NULL CHECK (gender IN ('female', 'male', 'other', 'undisclosed')),
    ADD COLUMN address                  TEXT NULL,
    ADD COLUMN emergency_contact_name   TEXT NULL,
    ADD COLUMN emergency_contact_phone  TEXT NULL;
