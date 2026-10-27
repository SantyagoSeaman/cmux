// Generated from cmux-tui/spec/sdk-schema.json. DO NOT EDIT.
package com.cmux.raw;


import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;


public final class ServerStatsJournalWriter implements WireValue {
    /** Events whose reply went before their journal commit (admit then reply, daemon setting admitThenReply). */
    private final Field<UInt64> admittedEvents;
    private final ServerStatsHistogram batchSize;
    private final UInt64 batches;
    private final UInt64 commitFailures;
    private final ServerStatsHistogram commitLockWaitUs;
    private final ServerStatsHistogram commitUs;
    private final UInt64 deadlineExpiries;
    private final UInt64 durableEvents;
    private final UInt64 durableQueued;
    /** Admitted events the journal writer did not commit: the journal lacks what their reply said. */
    private final Field<UInt64> journalDegraded;
    private final ServerStatsWriterPhase phase;
    private final UInt64 phaseForUs;
    /** Of read_barrier_waits, reads that went ahead at the wait's bound without those events. */
    private final Field<UInt64> readBarrierTimeouts;
    /** Journal reads that waited for events admitted before them. */
    private final Field<UInt64> readBarrierWaits;
    private final ServerStatsHistogram receiptWaitUs;
    private final UInt64 terminalEvents;
    private final UInt64 terminalQueued;

    private ServerStatsJournalWriter(Builder builder) {
        this.admittedEvents = builder.admittedEvents;
        if (!builder.batchSizeSet) throw new IllegalArgumentException("batch_size is required");
        this.batchSize = Wire.nonNull(builder.batchSize, "batch_size");
        if (!builder.batchesSet) throw new IllegalArgumentException("batches is required");
        this.batches = Wire.nonNull(builder.batches, "batches");
        if (!builder.commitFailuresSet) throw new IllegalArgumentException("commit_failures is required");
        this.commitFailures = Wire.nonNull(builder.commitFailures, "commit_failures");
        if (!builder.commitLockWaitUsSet) throw new IllegalArgumentException("commit_lock_wait_us is required");
        this.commitLockWaitUs = Wire.nonNull(builder.commitLockWaitUs, "commit_lock_wait_us");
        if (!builder.commitUsSet) throw new IllegalArgumentException("commit_us is required");
        this.commitUs = Wire.nonNull(builder.commitUs, "commit_us");
        if (!builder.deadlineExpiriesSet) throw new IllegalArgumentException("deadline_expiries is required");
        this.deadlineExpiries = Wire.nonNull(builder.deadlineExpiries, "deadline_expiries");
        if (!builder.durableEventsSet) throw new IllegalArgumentException("durable_events is required");
        this.durableEvents = Wire.nonNull(builder.durableEvents, "durable_events");
        if (!builder.durableQueuedSet) throw new IllegalArgumentException("durable_queued is required");
        this.durableQueued = Wire.nonNull(builder.durableQueued, "durable_queued");
        this.journalDegraded = builder.journalDegraded;
        if (!builder.phaseSet) throw new IllegalArgumentException("phase is required");
        this.phase = Wire.nonNull(builder.phase, "phase");
        if (!builder.phaseForUsSet) throw new IllegalArgumentException("phase_for_us is required");
        this.phaseForUs = Wire.nonNull(builder.phaseForUs, "phase_for_us");
        this.readBarrierTimeouts = builder.readBarrierTimeouts;
        this.readBarrierWaits = builder.readBarrierWaits;
        if (!builder.receiptWaitUsSet) throw new IllegalArgumentException("receipt_wait_us is required");
        this.receiptWaitUs = Wire.nonNull(builder.receiptWaitUs, "receipt_wait_us");
        if (!builder.terminalEventsSet) throw new IllegalArgumentException("terminal_events is required");
        this.terminalEvents = Wire.nonNull(builder.terminalEvents, "terminal_events");
        if (!builder.terminalQueuedSet) throw new IllegalArgumentException("terminal_queued is required");
        this.terminalQueued = Wire.nonNull(builder.terminalQueued, "terminal_queued");
    }

    public static Builder builder() { return new Builder(); }

    public Field<UInt64> admittedEvents() { return admittedEvents; }
    public ServerStatsHistogram batchSize() { return batchSize; }
    public UInt64 batches() { return batches; }
    public UInt64 commitFailures() { return commitFailures; }
    public ServerStatsHistogram commitLockWaitUs() { return commitLockWaitUs; }
    public ServerStatsHistogram commitUs() { return commitUs; }
    public UInt64 deadlineExpiries() { return deadlineExpiries; }
    public UInt64 durableEvents() { return durableEvents; }
    public UInt64 durableQueued() { return durableQueued; }
    public Field<UInt64> journalDegraded() { return journalDegraded; }
    public ServerStatsWriterPhase phase() { return phase; }
    public UInt64 phaseForUs() { return phaseForUs; }
    public Field<UInt64> readBarrierTimeouts() { return readBarrierTimeouts; }
    public Field<UInt64> readBarrierWaits() { return readBarrierWaits; }
    public ServerStatsHistogram receiptWaitUs() { return receiptWaitUs; }
    public UInt64 terminalEvents() { return terminalEvents; }
    public UInt64 terminalQueued() { return terminalQueued; }

    public static ServerStatsJournalWriter fromWire(Object value) {
        Map<String, Object> object = Wire.object(value, "ServerStatsJournalWriter");
        Builder builder = builder();
        Object rawAdmittedEvents = Wire.optional(object, "admitted_events");
        if (!Wire.isMissing(rawAdmittedEvents)) {
            builder.admittedEvents(Wire.uint64(rawAdmittedEvents, "ServerStatsJournalWriter.admitted_events"));
        }
        Object rawBatchSize = Wire.required(object, "batch_size");
        builder.batchSize(ServerStatsHistogram.fromWire(rawBatchSize));
        Object rawBatches = Wire.required(object, "batches");
        builder.batches(Wire.uint64(rawBatches, "ServerStatsJournalWriter.batches"));
        Object rawCommitFailures = Wire.required(object, "commit_failures");
        builder.commitFailures(Wire.uint64(rawCommitFailures, "ServerStatsJournalWriter.commit_failures"));
        Object rawCommitLockWaitUs = Wire.required(object, "commit_lock_wait_us");
        builder.commitLockWaitUs(ServerStatsHistogram.fromWire(rawCommitLockWaitUs));
        Object rawCommitUs = Wire.required(object, "commit_us");
        builder.commitUs(ServerStatsHistogram.fromWire(rawCommitUs));
        Object rawDeadlineExpiries = Wire.required(object, "deadline_expiries");
        builder.deadlineExpiries(Wire.uint64(rawDeadlineExpiries, "ServerStatsJournalWriter.deadline_expiries"));
        Object rawDurableEvents = Wire.required(object, "durable_events");
        builder.durableEvents(Wire.uint64(rawDurableEvents, "ServerStatsJournalWriter.durable_events"));
        Object rawDurableQueued = Wire.required(object, "durable_queued");
        builder.durableQueued(Wire.uint64(rawDurableQueued, "ServerStatsJournalWriter.durable_queued"));
        Object rawJournalDegraded = Wire.optional(object, "journal_degraded");
        if (!Wire.isMissing(rawJournalDegraded)) {
            builder.journalDegraded(Wire.uint64(rawJournalDegraded, "ServerStatsJournalWriter.journal_degraded"));
        }
        Object rawPhase = Wire.required(object, "phase");
        builder.phase(ServerStatsWriterPhase.fromWire(rawPhase));
        Object rawPhaseForUs = Wire.required(object, "phase_for_us");
        builder.phaseForUs(Wire.uint64(rawPhaseForUs, "ServerStatsJournalWriter.phase_for_us"));
        Object rawReadBarrierTimeouts = Wire.optional(object, "read_barrier_timeouts");
        if (!Wire.isMissing(rawReadBarrierTimeouts)) {
            builder.readBarrierTimeouts(Wire.uint64(rawReadBarrierTimeouts, "ServerStatsJournalWriter.read_barrier_timeouts"));
        }
        Object rawReadBarrierWaits = Wire.optional(object, "read_barrier_waits");
        if (!Wire.isMissing(rawReadBarrierWaits)) {
            builder.readBarrierWaits(Wire.uint64(rawReadBarrierWaits, "ServerStatsJournalWriter.read_barrier_waits"));
        }
        Object rawReceiptWaitUs = Wire.required(object, "receipt_wait_us");
        builder.receiptWaitUs(ServerStatsHistogram.fromWire(rawReceiptWaitUs));
        Object rawTerminalEvents = Wire.required(object, "terminal_events");
        builder.terminalEvents(Wire.uint64(rawTerminalEvents, "ServerStatsJournalWriter.terminal_events"));
        Object rawTerminalQueued = Wire.required(object, "terminal_queued");
        builder.terminalQueued(Wire.uint64(rawTerminalQueued, "ServerStatsJournalWriter.terminal_queued"));
        return builder.build();
    }

    @Override
    public Map<String, Object> toWire() {
        LinkedHashMap<String, Object> object = new LinkedHashMap<>();
        Wire.put(object, "admitted_events", admittedEvents);
        Wire.put(object, "batch_size", batchSize);
        Wire.put(object, "batches", batches);
        Wire.put(object, "commit_failures", commitFailures);
        Wire.put(object, "commit_lock_wait_us", commitLockWaitUs);
        Wire.put(object, "commit_us", commitUs);
        Wire.put(object, "deadline_expiries", deadlineExpiries);
        Wire.put(object, "durable_events", durableEvents);
        Wire.put(object, "durable_queued", durableQueued);
        Wire.put(object, "journal_degraded", journalDegraded);
        Wire.put(object, "phase", phase);
        Wire.put(object, "phase_for_us", phaseForUs);
        Wire.put(object, "read_barrier_timeouts", readBarrierTimeouts);
        Wire.put(object, "read_barrier_waits", readBarrierWaits);
        Wire.put(object, "receipt_wait_us", receiptWaitUs);
        Wire.put(object, "terminal_events", terminalEvents);
        Wire.put(object, "terminal_queued", terminalQueued);
        return Collections.unmodifiableMap(object);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof ServerStatsJournalWriter that)) return false;
        return Objects.equals(admittedEvents, that.admittedEvents) && Objects.equals(batchSize, that.batchSize) && Objects.equals(batches, that.batches) && Objects.equals(commitFailures, that.commitFailures) && Objects.equals(commitLockWaitUs, that.commitLockWaitUs) && Objects.equals(commitUs, that.commitUs) && Objects.equals(deadlineExpiries, that.deadlineExpiries) && Objects.equals(durableEvents, that.durableEvents) && Objects.equals(durableQueued, that.durableQueued) && Objects.equals(journalDegraded, that.journalDegraded) && Objects.equals(phase, that.phase) && Objects.equals(phaseForUs, that.phaseForUs) && Objects.equals(readBarrierTimeouts, that.readBarrierTimeouts) && Objects.equals(readBarrierWaits, that.readBarrierWaits) && Objects.equals(receiptWaitUs, that.receiptWaitUs) && Objects.equals(terminalEvents, that.terminalEvents) && Objects.equals(terminalQueued, that.terminalQueued);
    }

    @Override
    public int hashCode() { return Objects.hash(admittedEvents, batchSize, batches, commitFailures, commitLockWaitUs, commitUs, deadlineExpiries, durableEvents, durableQueued, journalDegraded, phase, phaseForUs, readBarrierTimeouts, readBarrierWaits, receiptWaitUs, terminalEvents, terminalQueued); }

    @Override
    public String toString() { return "ServerStatsJournalWriter" + toWire(); }

    public static final class Builder {
        private Field<UInt64> admittedEvents = Field.omitted();
        private ServerStatsHistogram batchSize;
        private boolean batchSizeSet;
        private UInt64 batches;
        private boolean batchesSet;
        private UInt64 commitFailures;
        private boolean commitFailuresSet;
        private ServerStatsHistogram commitLockWaitUs;
        private boolean commitLockWaitUsSet;
        private ServerStatsHistogram commitUs;
        private boolean commitUsSet;
        private UInt64 deadlineExpiries;
        private boolean deadlineExpiriesSet;
        private UInt64 durableEvents;
        private boolean durableEventsSet;
        private UInt64 durableQueued;
        private boolean durableQueuedSet;
        private Field<UInt64> journalDegraded = Field.omitted();
        private ServerStatsWriterPhase phase;
        private boolean phaseSet;
        private UInt64 phaseForUs;
        private boolean phaseForUsSet;
        private Field<UInt64> readBarrierTimeouts = Field.omitted();
        private Field<UInt64> readBarrierWaits = Field.omitted();
        private ServerStatsHistogram receiptWaitUs;
        private boolean receiptWaitUsSet;
        private UInt64 terminalEvents;
        private boolean terminalEventsSet;
        private UInt64 terminalQueued;
        private boolean terminalQueuedSet;

        public Builder admittedEvents(UInt64 value) {
            this.admittedEvents = Field.of(value);
            return this;
        }
        public Builder batchSize(ServerStatsHistogram value) {
            this.batchSize = value;
            this.batchSizeSet = true;
            return this;
        }
        public Builder batches(UInt64 value) {
            this.batches = value;
            this.batchesSet = true;
            return this;
        }
        public Builder commitFailures(UInt64 value) {
            this.commitFailures = value;
            this.commitFailuresSet = true;
            return this;
        }
        public Builder commitLockWaitUs(ServerStatsHistogram value) {
            this.commitLockWaitUs = value;
            this.commitLockWaitUsSet = true;
            return this;
        }
        public Builder commitUs(ServerStatsHistogram value) {
            this.commitUs = value;
            this.commitUsSet = true;
            return this;
        }
        public Builder deadlineExpiries(UInt64 value) {
            this.deadlineExpiries = value;
            this.deadlineExpiriesSet = true;
            return this;
        }
        public Builder durableEvents(UInt64 value) {
            this.durableEvents = value;
            this.durableEventsSet = true;
            return this;
        }
        public Builder durableQueued(UInt64 value) {
            this.durableQueued = value;
            this.durableQueuedSet = true;
            return this;
        }
        public Builder journalDegraded(UInt64 value) {
            this.journalDegraded = Field.of(value);
            return this;
        }
        public Builder phase(ServerStatsWriterPhase value) {
            this.phase = value;
            this.phaseSet = true;
            return this;
        }
        public Builder phaseForUs(UInt64 value) {
            this.phaseForUs = value;
            this.phaseForUsSet = true;
            return this;
        }
        public Builder readBarrierTimeouts(UInt64 value) {
            this.readBarrierTimeouts = Field.of(value);
            return this;
        }
        public Builder readBarrierWaits(UInt64 value) {
            this.readBarrierWaits = Field.of(value);
            return this;
        }
        public Builder receiptWaitUs(ServerStatsHistogram value) {
            this.receiptWaitUs = value;
            this.receiptWaitUsSet = true;
            return this;
        }
        public Builder terminalEvents(UInt64 value) {
            this.terminalEvents = value;
            this.terminalEventsSet = true;
            return this;
        }
        public Builder terminalQueued(UInt64 value) {
            this.terminalQueued = value;
            this.terminalQueuedSet = true;
            return this;
        }
        public ServerStatsJournalWriter build() { return new ServerStatsJournalWriter(this); }
    }
}
