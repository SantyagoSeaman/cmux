// Generated from cmux-tui/spec/sdk-schema.json. DO NOT EDIT.
package com.cmux.raw;


import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;


public final class JournalFrontendEventResult implements WireValue {
    /** True when the reply went before the journal commit: the event is checked and queued in journal order, and the writer commits it after the reply. Sent only to a client that declared frontend-journal-v2. */
    private final Field<Boolean> admitted;

    private JournalFrontendEventResult(Builder builder) {
        this.admitted = builder.admitted;
    }

    public static Builder builder() { return new Builder(); }

    public Field<Boolean> admitted() { return admitted; }
    public Boolean committed() { return true; }

    public static JournalFrontendEventResult fromWire(Object value) {
        Map<String, Object> object = Wire.object(value, "JournalFrontendEventResult");
        Builder builder = builder();
        Object rawAdmitted = Wire.optional(object, "admitted");
        if (!Wire.isMissing(rawAdmitted)) {
            builder.admitted(Wire.bool(rawAdmitted, "JournalFrontendEventResult.admitted"));
        }
        Object rawCommitted = Wire.required(object, "committed");
        ProtocolSupport.literal(rawCommitted, true, "JournalFrontendEventResult.committed");
        return builder.build();
    }

    @Override
    public Map<String, Object> toWire() {
        LinkedHashMap<String, Object> object = new LinkedHashMap<>();
        Wire.put(object, "admitted", admitted);
        Wire.put(object, "committed", true);
        return Collections.unmodifiableMap(object);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof JournalFrontendEventResult that)) return false;
        return Objects.equals(admitted, that.admitted);
    }

    @Override
    public int hashCode() { return Objects.hash(admitted); }

    @Override
    public String toString() { return "JournalFrontendEventResult" + toWire(); }

    public static final class Builder {
        private Field<Boolean> admitted = Field.omitted();

        public Builder admitted(Boolean value) {
            this.admitted = Field.of(value);
            return this;
        }
        public JournalFrontendEventResult build() { return new JournalFrontendEventResult(this); }
    }
}
