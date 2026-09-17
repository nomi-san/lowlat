//! The decoder's feed: which access units build, rebuild, tear down or feed a
//! decoder, and when the host is asked for a keyframe.
//!
//! Sans-IO. The policy of docs/10-client.md section 5 lives here and nothing
//! else does: a [`Decoder`] is handed units and reports what happened, and
//! this decides what that means. The thread that runs it and the backend it
//! runs against arrive with the first backend; the rules are testable now,
//! against a fake, and they are the part that has ever gone wrong.
//!
//! **A decoder is built from the stream, never from the configuration.** The
//! first unit led by a parameter set builds it, for the codec and depth the
//! header names; what the application asked for is a preference the host may
//! not have met, and a decoder built from it fails every picture of a stream
//! that differs.

use lowlat_core::video::{self, Codec, KeyframeMetadata, VIDEO_HEADER_LEN, VideoHeader};
pub use lowlat_decode::{Decoder, Fault, Fed};

/// What the feed did with one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The unit reached the decoder, with this outcome.
    Fed(Fed),
    /// A decoder was built for this unit, and it was fed.
    Built(Fed),
    /// The unit could not build a decoder and there was none to feed. Not an
    /// error and not a request: the next parameter-set-led unit builds one.
    Ignored,
    /// A keyframe-metadata message, consumed and never decoded.
    Consumed(KeyframeMetadata),
    /// The decoder faulted and was destroyed. **Ask the host for a keyframe,
    /// once.** The next unit that cannot build a decoder is ignored.
    Request,
    /// No decoder can serve this stream. The caller ends it.
    Failed,
}

/// The feed for one stream.
#[derive(Debug)]
pub struct Feed<D> {
    decoder: D,
    /// Whether a backend exists. The one bit every rule below turns on.
    present: bool,
    /// The generation the host last announced on the control channel, if any.
    /// A picture from an older generation comes from an encoder that no
    /// longer exists and tears the decoder down.
    announced: Option<u32>,
    /// What the present decoder was built for, so a codec that changes under
    /// it is a rebuild whatever the header's bit says.
    built_for: Option<(Codec, bool)>,
}

impl<D: Decoder> Feed<D> {
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            present: false,
            announced: None,
            built_for: None,
        }
    }

    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// The backend, for what the feed does not decide: taking the pictures
    /// it has ready.
    pub fn decoder_mut(&mut self) -> &mut D {
        &mut self.decoder
    }

    pub fn present(&self) -> bool {
        self.present
    }

    /// The host announced a generation for this stream.
    pub fn announce_generation(&mut self, generation: u32) {
        self.announced = Some(generation);
    }

    /// The application changed what it can decode. The decoder is torn down
    /// so the next keyframe builds one for the new declaration, and the host
    /// is asked for that keyframe.
    pub fn reconfigure(&mut self) -> Decision {
        self.teardown("configuration change");
        Decision::Request
    }

    /// One message off the video channel, header and all.
    pub fn feed(&mut self, content: &[u8]) -> Decision {
        let Ok(header) = video::parse(content) else {
            return Decision::Ignored;
        };
        if header.metadata {
            let Ok(metadata) = video::parse_metadata(content) else {
                return Decision::Ignored;
            };
            // **The rebuild signal, ahead of the keyframe it describes.** The
            // parameter sets on that keyframe are new, and the announced bit
            // it carries would otherwise keep the old decoder.
            if metadata.rebuilt {
                self.teardown("announced rebuild");
            }
            return Decision::Consumed(metadata);
        }

        let leads = video::leads_with_parameter_set(content, header.codec);
        let unit = content.get(VIDEO_HEADER_LEN..).unwrap_or(&[]);

        if header.announced {
            // The newer framing: a present decoder is fed whatever the unit
            // is and the generation rule does not apply; an absent one is
            // built from a parameter set and nothing else.
            return if self.present {
                self.feed_present(&header, unit, leads)
            } else if leads {
                self.build_and_feed(&header, unit, leads)
            } else {
                Decision::Ignored
            };
        }

        // The older framing: every parameter-set-led unit rebuilds, and a
        // picture from before the announced generation tears down first so
        // that generation's keyframe builds afresh.
        if self.present
            && self
                .announced
                .is_some_and(|announced| header.frame_id < announced)
        {
            self.teardown("stale generation");
        }
        if leads {
            if self.present {
                self.teardown("parameter set");
            }
            self.build_and_feed(&header, unit, leads)
        } else if self.present {
            self.feed_present(&header, unit, leads)
        } else {
            Decision::Ignored
        }
    }

    fn build_and_feed(&mut self, header: &VideoHeader, unit: &[u8], leads: bool) -> Decision {
        if self.decoder.build(header).is_err() {
            // Nothing to ask the host for: the next keyframe would fail the
            // same way, and asking for one per keyframe is a rebuild storm on
            // an established host.
            return Decision::Failed;
        }
        self.built(header);
        match self.feed_present(header, unit, leads) {
            Decision::Fed(fed) => Decision::Built(fed),
            other => other,
        }
    }

    fn built(&mut self, header: &VideoHeader) {
        self.present = true;
        self.built_for = Some((header.codec, header.ten_bit));
        lowlat_common::log_info!(
            "client: decoder built, codec={:?} ten_bit={} generation={}",
            header.codec,
            header.ten_bit,
            header.frame_id
        );
    }

    fn feed_present(&mut self, header: &VideoHeader, unit: &[u8], leads: bool) -> Decision {
        // A codec or depth the present decoder was not built for is a rebuild
        // whatever the header's bit says about parameter sets; the bit
        // promises the sets are known, not that the stream is the same.
        if leads && self.built_for != Some((header.codec, header.ten_bit)) {
            self.teardown("codec change");
            return self.build_and_feed(header, unit, leads);
        }
        match self.decoder.feed(unit) {
            Ok(Fed::FormatChanged) => {
                // **Once.** The unit that reported the change is fed to the
                // fresh decoder; a second report on the same unit is a fault
                // in the backend rather than a change in the stream.
                self.teardown("format change");
                if self.decoder.build(header).is_err() {
                    return Decision::Failed;
                }
                self.built(header);
                match self.decoder.feed(unit) {
                    Ok(Fed::FormatChanged) => self.fault(Fault::Unrecoverable),
                    Ok(fed) => Decision::Built(fed),
                    Err(fault) => self.fault(fault),
                }
            }
            Ok(fed) => Decision::Fed(fed),
            Err(fault) => self.fault(fault),
        }
    }

    /// **The request and the teardown are one act.** The decoder goes first,
    /// so every unit until the keyframe arrives finds none and is ignored
    /// rather than faulting again; that is what bounds requests to one per
    /// fault, whatever a burst of bad units after it looks like.
    fn fault(&mut self, fault: Fault) -> Decision {
        self.teardown("fault");
        match fault {
            Fault::Unrecoverable => Decision::Request,
            Fault::Fatal => Decision::Failed,
        }
    }

    fn teardown(&mut self, why: &str) {
        if self.present {
            self.decoder.destroy();
            self.present = false;
            self.built_for = None;
            lowlat_common::log_info!("client: decoder torn down, why={why}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_core::video::{Rotation, encode, encode_metadata};

    /// What a backend would have done, scripted per call.
    #[derive(Debug, Default)]
    struct Fake {
        built: u32,
        destroyed: u32,
        fed: Vec<Vec<u8>>,
        /// What the next feeds return, in order; `Picture` once exhausted.
        script: std::collections::VecDeque<Result<Fed, Fault>>,
        refuse_build: bool,
    }

    impl Decoder for Fake {
        fn build(&mut self, _header: &VideoHeader) -> Result<(), Fault> {
            if self.refuse_build {
                return Err(Fault::Fatal);
            }
            self.built += 1;
            Ok(())
        }
        fn feed(&mut self, unit: &[u8]) -> Result<Fed, Fault> {
            self.fed.push(unit.to_vec());
            self.script.pop_front().unwrap_or(Ok(Fed::Picture))
        }
        fn take(
            &mut self,
            _out: &mut lowlat_decode::Planes<'_>,
        ) -> Result<Option<lowlat_decode::Picture>, Fault> {
            Ok(None)
        }
        fn destroy(&mut self) {
            self.destroyed += 1;
        }
    }

    fn header(generation: u32, announced: bool) -> VideoHeader {
        VideoHeader {
            frame_id: generation,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced,
            metadata: false,
        }
    }

    fn message(generation: u32, announced: bool, unit_type: u8) -> Vec<u8> {
        let mut out = vec![0u8; VIDEO_HEADER_LEN];
        encode(&mut out, &header(generation, announced)).unwrap();
        out.extend_from_slice(&[0, 0, 0, 1, unit_type, 0xAA]);
        out
    }

    fn sps(generation: u32, announced: bool) -> Vec<u8> {
        message(generation, announced, 0x67)
    }

    fn delta(generation: u32, announced: bool) -> Vec<u8> {
        message(generation, announced, 0x41)
    }

    fn metadata(generation: u32, rebuilt: bool) -> Vec<u8> {
        let mut out = vec![0u8; video::METADATA_LEN];
        let meta = KeyframeMetadata {
            rebuilt,
            keyframe: true,
            ten_bit: false,
            chroma_444: false,
            rotation: Rotation::None,
        };
        encode_metadata(&mut out, &header(generation, true), &meta).unwrap();
        out
    }

    /// **A decoder waiting for a keyframe asks for nothing.** The host's own
    /// keyframe on seating and on a request is what it waits for; a request
    /// here would be an encoder rebuild on an established host for every
    /// unit that arrived before it.
    #[test]
    fn a_decoder_starved_of_a_keyframe_never_requests() {
        let mut feed = Feed::new(Fake::default());
        for _ in 0..50 {
            assert_eq!(feed.feed(&delta(1, false)), Decision::Ignored);
        }
        assert_eq!(feed.decoder().built, 0);
        assert!(!feed.present());
        assert!(matches!(
            feed.feed(&sps(1, false)),
            Decision::Built(Fed::Picture)
        ));
        assert_eq!(feed.feed(&delta(1, false)), Decision::Fed(Fed::Picture));
    }

    /// **The request is one act with the teardown**, and it happens at once.
    #[test]
    fn a_fault_requests_once_with_the_teardown() {
        let mut fake = Fake::default();
        fake.script.push_back(Ok(Fed::Picture));
        fake.script.push_back(Err(Fault::Unrecoverable));
        let mut feed = Feed::new(fake);
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(feed.feed(&delta(1, false)), Decision::Request);
        assert!(!feed.present(), "the decoder survived its own fault");
        assert_eq!(feed.decoder().destroyed, 1);
    }

    /// After the one request, every unit that cannot build a decoder is
    /// silent, however many there are, until a parameter set arrives.
    #[test]
    fn a_burst_of_bad_units_after_a_fault_requests_nothing_until_a_decoder_exists() {
        let mut fake = Fake::default();
        fake.script.push_back(Ok(Fed::Picture));
        fake.script.push_back(Err(Fault::Unrecoverable));
        let mut feed = Feed::new(fake);
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(feed.feed(&delta(1, false)), Decision::Request);
        let mut requests = 0;
        for _ in 0..200 {
            if feed.feed(&delta(1, false)) == Decision::Request {
                requests += 1;
            }
        }
        assert_eq!(requests, 0, "a burst after the fault sent more requests");
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(feed.decoder().built, 2);
    }

    /// Under the older framing every parameter set rebuilds; under the newer
    /// one the announced bit keeps the decoder, and the generation rule is
    /// skipped for it while an unannounced stale picture still tears down.
    #[test]
    fn bit_five_keeps_the_decoder_and_a_stale_generation_tears_it_down() {
        let mut feed = Feed::new(Fake::default());
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(
            feed.decoder().built,
            2,
            "a repeated parameter set did not rebuild"
        );
        assert_eq!(feed.decoder().destroyed, 1);

        assert_eq!(feed.feed(&sps(1, true)), Decision::Fed(Fed::Picture));
        assert_eq!(
            feed.decoder().built,
            2,
            "the announced bit did not keep the decoder"
        );

        feed.announce_generation(2);
        assert_eq!(
            feed.feed(&delta(1, true)),
            Decision::Fed(Fed::Picture),
            "an announced picture tripped the generation rule"
        );
        assert_eq!(feed.feed(&delta(1, false)), Decision::Ignored);
        assert!(
            !feed.present(),
            "a picture older than the announced generation kept the decoder"
        );
        assert_eq!(feed.decoder().destroyed, 2);
        assert!(matches!(feed.feed(&sps(2, false)), Decision::Built(_)));
    }

    /// The rebuild bit is what replaces the parameter-set rule: it tears the
    /// decoder down before the keyframe, and the keyframe, announced, builds
    /// a fresh one. A metadata message without it changes nothing.
    #[test]
    fn a_rebuild_bit_tears_down_before_the_keyframe() {
        let mut feed = Feed::new(Fake::default());
        assert!(matches!(feed.feed(&sps(1, true)), Decision::Built(_)));
        assert!(matches!(
            feed.feed(&metadata(1, false)),
            Decision::Consumed(_)
        ));
        assert!(feed.present());
        assert_eq!(feed.feed(&sps(1, true)), Decision::Fed(Fed::Picture));

        assert!(matches!(
            feed.feed(&metadata(2, true)),
            Decision::Consumed(_)
        ));
        assert!(!feed.present(), "the rebuild bit left the decoder standing");
        assert_eq!(feed.decoder().fed.len(), 2, "metadata reached the decoder");
        assert!(matches!(feed.feed(&sps(2, true)), Decision::Built(_)));
        assert_eq!(feed.decoder().built, 2);
    }

    /// A format change re-feeds the unit once to a fresh decoder, so the
    /// picture that changed the format is not lost.
    #[test]
    fn a_format_change_rebuilds_and_refeeds_the_unit_once() {
        let mut fake = Fake::default();
        fake.script.push_back(Ok(Fed::Picture));
        fake.script.push_back(Ok(Fed::FormatChanged));
        let mut feed = Feed::new(fake);
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(feed.feed(&delta(1, false)), Decision::Built(Fed::Picture));
        assert_eq!(feed.decoder().built, 2);
        assert_eq!(feed.decoder().fed.len(), 3, "the unit was not re-fed");
        assert_eq!(feed.decoder().fed[1], feed.decoder().fed[2]);
    }

    /// A backend that cannot be built ends the stream rather than asking the
    /// host for keyframes it would fail on the same way.
    #[test]
    fn a_decoder_that_cannot_be_built_fails_without_a_request() {
        let mut feed = Feed::new(Fake {
            refuse_build: true,
            ..Fake::default()
        });
        assert_eq!(feed.feed(&sps(1, false)), Decision::Failed);
        assert_eq!(feed.feed(&sps(1, true)), Decision::Failed);
    }

    /// A reconfiguration is the other trigger: teardown and one request.
    #[test]
    fn a_reconfiguration_tears_down_and_requests() {
        let mut feed = Feed::new(Fake::default());
        assert!(matches!(feed.feed(&sps(1, false)), Decision::Built(_)));
        assert_eq!(feed.reconfigure(), Decision::Request);
        assert!(!feed.present());
        assert_eq!(feed.feed(&delta(1, false)), Decision::Ignored);
    }
}
