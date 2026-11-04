-- Apps whose copies are never stored, in addition to the built-in password-manager list.
-- Names are normalised: lower case, no folder, no ".exe" (e.g. "keepassxc").
CREATE TABLE excluded_app (
    name TEXT PRIMARY KEY
);
