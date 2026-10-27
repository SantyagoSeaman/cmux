// Generated from cmux-tui/spec/sdk-schema.json. DO NOT EDIT.
package com.cmux.raw;


import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;


public final class CancelStreamResult implements WireValue {
    private final CancelStreamResultOutcome outcome;

    private CancelStreamResult(Builder builder) {
        if (!builder.outcomeSet) throw new IllegalArgumentException("outcome is required");
        this.outcome = Wire.nonNull(builder.outcome, "outcome");
    }

    public static Builder builder() { return new Builder(); }

    public CancelStreamResultOutcome outcome() { return outcome; }

    public static CancelStreamResult fromWire(Object value) {
        Map<String, Object> object = Wire.object(value, "CancelStreamResult");
        Builder builder = builder();
        Object rawOutcome = Wire.required(object, "outcome");
        builder.outcome(CancelStreamResultOutcome.fromWire(rawOutcome));
        return builder.build();
    }

    @Override
    public Map<String, Object> toWire() {
        LinkedHashMap<String, Object> object = new LinkedHashMap<>();
        Wire.put(object, "outcome", outcome);
        return Collections.unmodifiableMap(object);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof CancelStreamResult that)) return false;
        return Objects.equals(outcome, that.outcome);
    }

    @Override
    public int hashCode() { return Objects.hash(outcome); }

    @Override
    public String toString() { return "CancelStreamResult" + toWire(); }

    public static final class Builder {
        private CancelStreamResultOutcome outcome;
        private boolean outcomeSet;

        public Builder outcome(CancelStreamResultOutcome value) {
            this.outcome = value;
            this.outcomeSet = true;
            return this;
        }
        public CancelStreamResult build() { return new CancelStreamResult(this); }
    }
}
