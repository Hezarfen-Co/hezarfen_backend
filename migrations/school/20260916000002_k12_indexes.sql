-- Indexes the K12 remodel's drop/recreate left out. `enrollment_user` existed
-- before the remodel and was dropped with its table; the other two key
-- hot reads the remodel added.
CREATE INDEX enrollment_user           ON enrollment (app_user);
CREATE INDEX course_membership_user    ON course_membership (app_user);
CREATE INDEX exam_audience_class_course ON exam_audience (class_course);
