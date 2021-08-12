//! Implements a parser for the PSI-MS mzML and indexedmzML XML file formats
//! for representing raw and processed mass spectra.

use std::convert::TryInto;
use std::fs;
use std::io;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::marker::PhantomData;

use log::warn;

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Error as XMLError;
use quick_xml::Reader;

use super::offset_index::OffsetIndex;
use super::traits::{
    MZFileReader, RandomAccessScanIterator, ScanAccessError, ScanSource, SeekRead,
};

use mzpeaks::{CentroidPeak, DeconvolutedPeak};

use crate::params::{Param, ParamList};
use crate::spectrum::scan_properties::*;
use crate::spectrum::signal::{
    ArrayType, BinaryArrayMap, BinaryCompressionType, BinaryDataArrayType, DataArray,
};
use crate::spectrum::spectrum::{
    CentroidPeakAdapting, CentroidSpectrumType, DeconvolutedPeakAdapting, MultiLayerSpectrum,
    RawSpectrum, Spectrum,
};
use crate::SpectrumBehavior;

pub type Bytes = Vec<u8>;

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum MzMLParserState {
    Start = 0,
    Resume,

    // Top-level metadata
    CVList,
    FileDescription,
    FileContents,
    SourceFileList,
    SourceFile,

    ReferenceParamGroupList,
    ReferenceParamGroup,

    SoftwareList,
    Software,

    InstrumentConfigurationList,
    InstrumentConfiguration,
    ComponentList,
    Source,
    Analyzer,
    Detector,

    DataProcessingList,
    DataProcessing,
    ProcessingMethod,

    // Spectrum and Chromatogram List Elements
    Spectrum,
    SpectrumDone,

    SpectrumList,
    SpectrumListDone,

    BinaryDataArrayList,
    BinaryDataArray,
    Binary,

    ScanList,
    Scan,
    ScanWindowList,
    ScanWindow,

    PrecursorList,
    Precursor,
    IsolationWindow,
    SelectedIonList,
    SelectedIon,
    Activation,

    Chromatogram,
    ChromatogramDone,

    ParserError,
}

#[derive(Debug, Clone)]
pub enum MzMLParserError {
    NoError,
    UnknownError(MzMLParserState),
    IncompleteSpectrum,
    IncompleteElementError(String, MzMLParserState),
    XMLError(MzMLParserState, String),
}

impl Default for MzMLParserError {
    fn default() -> MzMLParserError {
        MzMLParserError::NoError
    }
}

const BUFFER_SIZE: usize = 10000;

#[derive(Default)]
struct MzMLSpectrumBuilder<
    C: CentroidPeakAdapting = CentroidPeak,
    D: DeconvolutedPeakAdapting = DeconvolutedPeak,
> {
    pub params: ParamList,
    pub acquisition: Acquisition,
    pub precursor: Precursor,

    pub arrays: BinaryArrayMap,
    pub current_array: DataArray,

    pub index: usize,
    pub scan_id: String,
    pub ms_level: u8,
    pub polarity: ScanPolarity,
    pub signal_continuity: SignalContinuity,
    pub has_precursor: bool,
    centroid_type: PhantomData<C>,
    deconvoluted_type: PhantomData<D>,
}

pub trait SpectrumBuilding<
    C: CentroidPeakAdapting,
    D: DeconvolutedPeakAdapting,
    S: SpectrumBehavior<C, D>,
>
{
    fn isolation_window_mut(&mut self) -> &mut IsolationWindow;
    fn scan_window_mut(&mut self) -> &mut ScanWindow;
    fn selected_ion_mut(&mut self) -> &mut SelectedIon;
    fn current_array_mut(&mut self) -> &mut DataArray;
    fn into_spectrum(self, spectrum: &mut S);

    fn fill_binary_data_array(&mut self, param: Param) {
        match param.name.as_ref() {
            // Compression types
            "zlib compression" => {
                self.current_array_mut().compression = BinaryCompressionType::Zlib;
            }
            "no compression" => {
                self.current_array_mut().compression = BinaryCompressionType::NoCompression;
            }

            // Array data types
            "64-bit float" => {
                self.current_array_mut().dtype = BinaryDataArrayType::Float64;
            }
            "32-bit float" => {
                self.current_array_mut().dtype = BinaryDataArrayType::Float32;
            }
            "64-bit integer" => {
                self.current_array_mut().dtype = BinaryDataArrayType::Int64;
            }
            "32-bit integer" => {
                self.current_array_mut().dtype = BinaryDataArrayType::Int32;
            }
            "null-terminated ASCII string" => {
                self.current_array_mut().dtype = BinaryDataArrayType::ASCII;
            }

            // Array types
            "m/z array" => self.current_array_mut().name = ArrayType::MZArray,
            "intensity array" => self.current_array_mut().name = ArrayType::IntensityArray,
            "charge array" => self.current_array_mut().name = ArrayType::ChargeArray,
            "non-standard data array" => {
                self.current_array_mut().name =
                    ArrayType::NonStandardDataArray { name: param.value };
            }
            "mean ion mobility array"
            | "mean drift time array"
            | "mean inverse reduced ion mobility array" => {
                self.current_array_mut().name = ArrayType::MeanIonMobilityArray
            }
            "ion mobility array" | "drift time array" | "inverse reduced ion mobility array" => {
                self.current_array_mut().name = ArrayType::IonMobilityArray
            }
            "deconvoluted ion mobility array"
            | "deconvoluted drift time array"
            | "deconvoluted inverse reduced ion mobility array" => {
                self.current_array_mut().name = ArrayType::DeconvolutedIonMobilityArray
            }

            &_ => {
                self.current_array_mut().params.push(param);
            }
        }
    }

    fn fill_selected_ion(&mut self, param: Param) {
        match param.name.as_ref() {
            "selected ion m/z" => {
                self.selected_ion_mut().mz = param.coerce().expect("Failed to parse ion m/z");
            }
            "peak intensity" => {
                self.selected_ion_mut().intensity =
                    param.coerce().expect("Failed to parse peak intensity");
            }
            "charge state" => {
                self.selected_ion_mut().charge =
                    Some(param.coerce().expect("Failed to parse ion charge"));
            }
            &_ => {
                self.selected_ion_mut().params.push(param);
            }
        };
    }

    fn fill_isolation_window(&mut self, param: Param) {
        let window = self.isolation_window_mut();
        match param.name.as_ref() {
            "isolation window target m/z" => {
                window.target = param
                    .coerce()
                    .expect("Failed to parse isolation window target");
                window.flags = match window.flags {
                    IsolationWindowState::Unknown => IsolationWindowState::Complete,
                    IsolationWindowState::Explicit => IsolationWindowState::Complete,
                    IsolationWindowState::Offset => {
                        window.lower_bound = window.target - window.lower_bound;
                        window.upper_bound += window.target;
                        IsolationWindowState::Complete
                    }
                    IsolationWindowState::Complete => IsolationWindowState::Complete,
                };
            }
            "isolation window lower offset" => {
                let lower_bound: f64 = param
                    .coerce()
                    .expect("Failed to parse isolation window limit");
                match window.flags {
                    IsolationWindowState::Unknown => {
                        window.flags = IsolationWindowState::Offset;
                        window.lower_bound = lower_bound;
                    }
                    IsolationWindowState::Complete => {
                        window.lower_bound = window.target - lower_bound;
                    }
                    _ => {}
                }
            }
            "isolation window upper offset" => {
                let upper_bound: f64 = param
                    .coerce()
                    .expect("Failed to parse isolation window limit");
                match window.flags {
                    IsolationWindowState::Unknown => {
                        window.flags = IsolationWindowState::Offset;
                        window.upper_bound = upper_bound;
                    }
                    IsolationWindowState::Complete => {
                        window.upper_bound = window.target + upper_bound;
                    }
                    _ => {}
                }
            }
            "isolation window lower limit" => {
                let lower_bound: f64 = param
                    .coerce()
                    .expect("Failed to parse isolation window limit");
                match window.flags {
                    IsolationWindowState::Unknown => {
                        window.flags = IsolationWindowState::Explicit;
                        window.lower_bound = lower_bound;
                    }
                    _ => {}
                }
            }
            "isolation window upper limit" => {
                let upper_bound: f64 = param
                    .coerce()
                    .expect("Failed to parse isolation window limit");
                match window.flags {
                    IsolationWindowState::Unknown => {
                        window.flags = IsolationWindowState::Explicit;
                        window.upper_bound = upper_bound;
                    }
                    _ => {}
                }
            }
            &_ => {}
        }
    }

    fn fill_scan_window(&mut self, param: Param) {
        let window = self.scan_window_mut();
        match param.name.as_ref() {
            "scan window lower limit" => {
                window.lower_bound = param.coerce().expect("Failed to parse scan window limit");
            }
            "scan window upper limit" => {
                window.upper_bound = param.coerce().expect("Failed to parse scan window limit");
            }
            &_ => {}
        }
    }
}

pub type ParserResult = Result<MzMLParserState, MzMLParserError>;

impl<C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting>
    SpectrumBuilding<C, D, MultiLayerSpectrum<C, D>> for MzMLSpectrumBuilder<C, D>
{
    fn isolation_window_mut(&mut self) -> &mut IsolationWindow {
        &mut self.precursor.isolation_window
    }

    fn scan_window_mut(&mut self) -> &mut ScanWindow {
        self.acquisition
            .scans
            .last_mut()
            .unwrap()
            .scan_windows
            .last_mut()
            .unwrap()
    }

    fn selected_ion_mut(&mut self) -> &mut SelectedIon {
        &mut self.precursor.ion
    }

    fn current_array_mut(&mut self) -> &mut DataArray {
        &mut self.current_array
    }

    fn into_spectrum(self, spectrum: &mut MultiLayerSpectrum<C, D>) {
        let description = &mut spectrum.description;

        description.id = self.scan_id;
        description.index = self.index;
        description.signal_continuity = self.signal_continuity;
        description.ms_level = self.ms_level;
        description.polarity = self.polarity;

        description.params = self.params;
        description.acquisition = self.acquisition;
        if self.has_precursor {
            description.precursor = Some(self.precursor);
        } else {
            description.precursor = None;
        }

        spectrum.arrays = Some(self.arrays);
    }
}

impl<C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting> MzMLSpectrumBuilder<C, D> {
    pub fn new() -> MzMLSpectrumBuilder<C, D> {
        MzMLSpectrumBuilder {
            ..Default::default()
        }
    }

    pub fn _reset(&mut self) {
        self.params.clear();
        self.acquisition = Acquisition::default();
        self.arrays.clear();
        self.current_array.clear();
        self.scan_id.clear();

        self.precursor = Precursor::default();
        self.index = 0;
        self.has_precursor = false;
        self.signal_continuity = SignalContinuity::Unknown;
        self.polarity = ScanPolarity::Unknown;
    }

    pub fn _to_spectrum(&self, spectrum: &mut MultiLayerSpectrum<C, D>) {
        let description = &mut spectrum.description;

        description.id = self.scan_id.clone();
        description.index = self.index;
        description.signal_continuity = self.signal_continuity;
        description.ms_level = self.ms_level;
        description.polarity = self.polarity;

        description.params = self.params.clone();
        description.acquisition = self.acquisition.clone();
        if self.has_precursor {
            description.precursor = Some(self.precursor.clone());
        } else {
            description.precursor = None;
        }

        spectrum.arrays = Some(self.arrays.clone());
    }

    fn handle_param<B: io::BufRead>(
        &self,
        event: &BytesStart,
        reader: &Reader<B>,
        state: MzMLParserState,
    ) -> Result<Param, MzMLParserError> {
        let mut param = Param::new();
        for attr_parsed in event.attributes() {
            match attr_parsed {
                Ok(attr) => match attr.key {
                    b"name" => {
                        param.name = attr.unescape_and_decode_value(reader).expect(&format!(
                            "Error decoding CV param name at {}",
                            reader.buffer_position()
                        ));
                    }
                    b"value" => {
                        param.value = attr.unescape_and_decode_value(reader).expect(&format!(
                            "Error decoding CV param value at {}",
                            reader.buffer_position()
                        ));
                    }
                    b"cvRef" => {
                        param.controlled_vocabulary =
                            Some(attr.unescape_and_decode_value(reader).expect(&format!(
                                "Error decoding CV param reference at {}",
                                reader.buffer_position()
                            )));
                    }
                    b"accession" => {
                        param.accession = attr.unescape_and_decode_value(reader).expect(&format!(
                            "Error decoding CV param accession at {}",
                            reader.buffer_position()
                        ));
                    }
                    b"unitName" => {
                        param.unit_info =
                            Some(attr.unescape_and_decode_value(reader).expect(&format!(
                                "Error decoding CV param unit name at {}",
                                reader.buffer_position()
                            )));
                    }
                    b"unitAccession" => {}
                    b"unitCvRef" => {}
                    _ => {}
                },
                Err(msg) => return Err(self.handle_xml_error(msg, state)),
            }
        }
        Ok(param)
    }

    pub fn handle_xml_error(
        &self,
        error: quick_xml::Error,
        state: MzMLParserState,
    ) -> MzMLParserError {
        MzMLParserError::XMLError(state, format!("{:?}", error))
    }

    pub fn fill_spectrum(&mut self, param: Param) {
        match param.name.as_ref() {
            "ms level" => {
                self.ms_level = param.coerce().expect("Failed to parse ms level");
            }
            "positive scan" => {
                self.polarity = ScanPolarity::Positive;
            }
            "negative scan" => {
                self.polarity = ScanPolarity::Negative;
            }
            "profile spectrum" => {
                self.signal_continuity = SignalContinuity::Profile;
            }
            "centroid spectrum" => {
                self.signal_continuity = SignalContinuity::Centroid;
            }
            &_ => {
                self.params.push(param);
            }
        };
    }

    pub fn fill_param_into(&mut self, param: Param, state: MzMLParserState) {
        match state {
            MzMLParserState::Spectrum => {
                self.fill_spectrum(param);
            }
            MzMLParserState::ScanList => self.acquisition.params.push(param),
            MzMLParserState::Scan => self
                .acquisition
                .scans
                .last_mut()
                .unwrap()
                .params
                .push(param),
            MzMLParserState::ScanWindowList => self
                .acquisition
                .scans
                .last_mut()
                .unwrap()
                .params
                .push(param),
            MzMLParserState::ScanWindow => {
                self.fill_scan_window(param);
            }
            MzMLParserState::IsolationWindow => {
                self.fill_isolation_window(param);
            }
            MzMLParserState::SelectedIon | MzMLParserState::SelectedIonList => {
                self.fill_selected_ion(param);
            }
            MzMLParserState::Activation => match param.name.as_ref() {
                "collision energy" | "activation energy" => {
                    self.precursor.activation.energy =
                        param.coerce().expect("Failed to parse collision energy");
                }
                &_ => {
                    self.precursor.activation.params.push(param);
                }
            },
            MzMLParserState::BinaryDataArrayList => {}
            MzMLParserState::BinaryDataArray => {
                self.fill_binary_data_array(param);
            }
            MzMLParserState::Precursor | MzMLParserState::PrecursorList => {
                self.precursor.params.push(param);
            }
            _ => {}
        };
    }

    pub fn start_element<B: io::BufRead>(
        &mut self,
        event: &BytesStart,
        state: MzMLParserState,
        reader: &Reader<B>,
    ) -> ParserResult {
        let elt_name = event.name();
        match elt_name {
            b"spectrum" => {
                for attr_parsed in event.attributes() {
                    match attr_parsed {
                        Ok(attr) => match attr.key {
                            b"id" => {
                                self.scan_id = attr
                                    .unescape_and_decode_value(reader)
                                    .expect("Error decoding id");
                            }
                            b"index" => {
                                self.index = (&String::from_utf8_lossy(&attr.value))
                                    .parse::<usize>()
                                    .expect("Failed to parse index");
                            }
                            _ => {}
                        },
                        Err(msg) => {
                            return Err(self.handle_xml_error(msg, state));
                        }
                    }
                }
                return Ok(MzMLParserState::Spectrum);
            }
            b"spectrumList" => {
                return Ok(MzMLParserState::SpectrumList);
            }
            b"scanList" => {
                return Ok(MzMLParserState::ScanList);
            }
            b"scan" => {
                let mut scan_event = ScanEvent::default();
                for attr_parsed in event.attributes() {
                    match attr_parsed {
                        Ok(attr) => {
                            if attr.key == b"instrumentConfigurationRef" {
                                scan_event.instrument_configuration_id = attr
                                    .unescape_and_decode_value(reader)
                                    .expect("Error decoding id");
                            }
                        }
                        Err(msg) => {
                            return Err(self.handle_xml_error(msg, state));
                        }
                    }
                }
                self.acquisition.scans.push(scan_event);
                return Ok(MzMLParserState::Scan);
            }
            b"scanWindow" => {
                let window = ScanWindow::default();
                self.acquisition
                    .scans
                    .last_mut()
                    .expect("Scan window without scan")
                    .scan_windows
                    .push(window);
                return Ok(MzMLParserState::ScanWindow);
            }
            b"scanWindowList" => {
                return Ok(MzMLParserState::ScanWindowList);
            }
            b"precursorList" => {
                return Ok(MzMLParserState::PrecursorList);
            }
            b"precursor" => {
                self.has_precursor = true;
                for attr_parsed in event.attributes() {
                    match attr_parsed {
                        Ok(attr) => {
                            if attr.key == b"spectrumRef" {
                                self.precursor.precursor_id = attr
                                    .unescape_and_decode_value(reader)
                                    .expect("Error decoding id");
                            }
                        }
                        Err(msg) => {
                            return Err(self.handle_xml_error(msg, state));
                        }
                    }
                }
                return Ok(MzMLParserState::Precursor);
            }
            b"isolationWindow" => {
                return Ok(MzMLParserState::IsolationWindow);
            }
            b"selectedIonList" => {
                return Ok(MzMLParserState::SelectedIonList);
            }
            b"selectedIon" => {
                return Ok(MzMLParserState::SelectedIon);
            }
            b"activation" => {
                return Ok(MzMLParserState::Activation);
            }
            b"binaryDataArrayList" => {
                return Ok(MzMLParserState::BinaryDataArrayList);
            }
            b"binaryDataArray" => {
                return Ok(MzMLParserState::BinaryDataArray);
            }
            b"binary" => {
                return Ok(MzMLParserState::Binary);
            }
            _ => {}
        };
        Ok(state)
    }

    pub fn empty_element<B: io::BufRead>(
        &mut self,
        event: &BytesStart,
        state: MzMLParserState,
        reader: &Reader<B>,
    ) -> ParserResult {
        let elt_name = event.name();
        match elt_name {
            b"cvParam" | b"userParam" => match self.handle_param(event, reader, state) {
                Ok(param) => {
                    self.fill_param_into(param, state);
                    return Ok(state);
                }
                Err(err) => return Err(err),
            },
            &_ => {}
        }
        Ok(state)
    }

    pub fn end_element(&mut self, event: &BytesEnd, state: MzMLParserState) -> ParserResult {
        let elt_name = event.name();
        match elt_name {
            b"spectrum" => return Ok(MzMLParserState::SpectrumDone),
            b"scanList" => return Ok(MzMLParserState::Spectrum),
            b"scan" => return Ok(MzMLParserState::ScanList),
            b"scanWindow" => return Ok(MzMLParserState::ScanWindowList),
            b"scanWindowList" => return Ok(MzMLParserState::Scan),
            b"precursorList" => return Ok(MzMLParserState::Spectrum),
            b"precursor" => return Ok(MzMLParserState::PrecursorList),
            b"isolationWindow" => return Ok(MzMLParserState::Precursor),
            b"selectedIonList" => return Ok(MzMLParserState::Precursor),
            b"selectedIon" => return Ok(MzMLParserState::SelectedIonList),
            b"activation" => return Ok(MzMLParserState::Precursor),
            b"binaryDataArrayList" => {
                return Ok(MzMLParserState::Spectrum);
            }
            b"binaryDataArray" => {
                let mut array = self.current_array.clone();
                array
                    .decode_and_store()
                    .expect("Error during decoding and storing of array data");
                self.arrays.add(array);
                self.current_array.clear();
                return Ok(MzMLParserState::BinaryDataArrayList);
            }
            b"binary" => return Ok(MzMLParserState::BinaryDataArray),
            b"spectrumList" => return Ok(MzMLParserState::SpectrumListDone),
            _ => {}
        };
        Ok(state)
    }

    pub fn text(&mut self, event: &BytesText, state: MzMLParserState) -> ParserResult {
        if state == MzMLParserState::Binary {
            let bin = event
                .unescaped()
                .expect("Failed to unescape binary data array content");
            self.current_array.data = Bytes::from(&*bin);
        }
        Ok(state)
    }
}

impl<C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting> Into<CentroidSpectrumType<C>>
    for MzMLSpectrumBuilder<C, D>
{
    fn into(self) -> CentroidSpectrumType<C> {
        let mut spec = MultiLayerSpectrum::<C, D>::default();
        self.into_spectrum(&mut spec);
        spec.try_into().unwrap()
    }
}

impl Into<Spectrum> for MzMLSpectrumBuilder {
    fn into(self) -> Spectrum {
        let mut spec = Spectrum::default();
        self.into_spectrum(&mut spec);
        spec
    }
}

impl Into<RawSpectrum> for MzMLSpectrumBuilder {
    fn into(self) -> RawSpectrum {
        let mut spec = Spectrum::default();
        self.into_spectrum(&mut spec);
        spec.into()
    }
}

/// An mzML parser that supports iteration and random access. The parser produces
/// [`Spectrum`] instances, which may be converted to [`RawSpectrum`](crate::spectrum::spectrum::RawSpectrum)
/// or [`CentroidSpectrum`](crate::spectrum::CentroidSpectrum) as is appropriate to the data.
///
/// When the readable stream the parser is wrapped around supports [`io::Seek`],
/// additional random access operations are available.
pub struct MzMLReaderType<
    R: Read,
    C: CentroidPeakAdapting = CentroidPeak,
    D: DeconvolutedPeakAdapting = DeconvolutedPeak,
> {
    pub state: MzMLParserState,
    pub handle: BufReader<R>,
    pub error: MzMLParserError,
    pub index: OffsetIndex,
    buffer: Bytes,
    centroid_type: PhantomData<C>,
    deconvoluted_type: PhantomData<D>,
}

impl<R: Read, C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting> MzMLReaderType<R, C, D> {
    pub fn new(file: R) -> MzMLReaderType<R, C, D> {
        let handle = BufReader::with_capacity(BUFFER_SIZE, file);
        MzMLReaderType {
            handle,
            state: MzMLParserState::Start,
            error: MzMLParserError::default(),
            buffer: Bytes::new(),
            index: OffsetIndex::new("spectrum".to_owned()),
            centroid_type: PhantomData,
            deconvoluted_type: PhantomData,
        }
    }

    fn _parse_into(
        &mut self,
        accumulator: &mut MzMLSpectrumBuilder<C, D>,
    ) -> Result<usize, MzMLParserError> {
        let mut reader = Reader::from_reader(&mut self.handle);
        reader.trim_text(true);
        let mut offset: usize = 0;
        loop {
            match reader.read_event(&mut self.buffer) {
                Ok(Event::Start(ref e)) => {
                    match accumulator.start_element(e, self.state, &reader) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::End(ref e)) => {
                    match accumulator.end_element(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Text(ref e)) => {
                    match accumulator.text(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Empty(ref e)) => {
                    match accumulator.empty_element(e, self.state, &reader) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    }
                }
                Ok(Event::Eof) => {
                    break;
                }
                Err(err) => match &err {
                    XMLError::EndEventMismatch {
                        expected,
                        found: _found,
                    } => {
                        if expected.is_empty() && self.state == MzMLParserState::Resume {
                            continue;
                        } else {
                            self.error = MzMLParserError::IncompleteElementError(
                                String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                                self.state,
                            );
                            self.state = MzMLParserState::ParserError;
                        }
                    }
                    _ => {
                        self.error = MzMLParserError::IncompleteElementError(
                            String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                            self.state,
                        );
                        self.state = MzMLParserState::ParserError;
                    }
                },
                _ => {}
            };
            offset += self.buffer.len();
            self.buffer.clear();
            match self.state {
                MzMLParserState::SpectrumDone | MzMLParserState::ParserError => {
                    break;
                }
                _ => {}
            };
        }
        match self.state {
            MzMLParserState::SpectrumDone => Ok(offset),
            MzMLParserState::ParserError => Err(self.error.clone()),
            _ => Err(MzMLParserError::IncompleteSpectrum),
        }
    }

    /// Populate a new [`Spectrum`] in-place on the next available spectrum data.
    /// This allocates memory to build the spectrum's attributes but then moves it
    /// into `spectrum` rather than copying it.
    pub fn read_into(
        &mut self,
        spectrum: &mut MultiLayerSpectrum<C, D>,
    ) -> Result<usize, MzMLParserError> {
        let mut accumulator = MzMLSpectrumBuilder::<C, D>::new();
        if self.state == MzMLParserState::SpectrumDone {
            self.state = MzMLParserState::Resume;
        }
        match self._parse_into(&mut accumulator) {
            Ok(sz) => {
                accumulator.into_spectrum(spectrum);
                Ok(sz)
            }
            Err(err) => Err(err),
        }
    }

    /// Read the next spectrum directly. Used to implement iteration.
    pub fn read_next(&mut self) -> Option<MultiLayerSpectrum<C, D>> {
        let mut spectrum = MultiLayerSpectrum::<C, D>::default();
        match self.read_into(&mut spectrum) {
            Ok(_sz) => Some(spectrum.into()),
            Err(_err) => None,
        }
    }
}

/// [`MzMLReaderType`] instances are [`Iterator`]s over [`Spectrum`]
impl<R: io::Read, C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting> Iterator
    for MzMLReaderType<R, C, D>
{
    type Item = MultiLayerSpectrum<C, D>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_next()
    }
}

/// They can also be used to fetch specific spectra by ID, index, or start
/// time when the underlying file stream supports [`io::Seek`].
impl<R: io::Read + io::Seek, C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting>
    ScanSource<C, D, MultiLayerSpectrum<C, D>> for MzMLReaderType<R, C, D>
{
    /// Retrieve a spectrum by it's native ID
    fn get_spectrum_by_id(&mut self, id: &str) -> Option<MultiLayerSpectrum<C, D>> {
        let offset_ref = self.index.get(id);
        let offset = offset_ref.expect("Failed to retrieve offset");
        let start = self
            .handle
            .stream_position()
            .expect("Failed to save checkpoint");
        self.seek(SeekFrom::Start(offset))
            .expect("Failed to move seek to offset");
        let result = self.read_next();
        self.seek(SeekFrom::Start(start))
            .expect("Failed to restore offset");
        result
    }

    /// Retrieve a spectrum by it's integer index
    fn get_spectrum_by_index(&mut self, index: usize) -> Option<MultiLayerSpectrum<C, D>> {
        let (_id, offset) = self.index.get_index(index)?;
        let byte_offset = offset;
        let start = self
            .handle
            .stream_position()
            .expect("Failed to save checkpoint");
        self.seek(SeekFrom::Start(byte_offset)).ok()?;
        let result = self.read_next();
        self.seek(SeekFrom::Start(start))
            .expect("Failed to restore offset");
        result
    }

    /// Return the data stream to the beginning
    fn reset(&mut self) -> &Self {
        self.seek(SeekFrom::Start(0))
            .expect("Failed to reset file stream");
        self
    }

    fn get_index(&self) -> &OffsetIndex {
        if !self.index.init {
            warn!("Attempting to use an uninitialized offset index on MzMLReaderType")
        }
        &self.index
    }

    fn set_index(&mut self, index: OffsetIndex) {
        self.index = index
    }
}

/// The iterator can also be updated to move to a different location in the
/// stream efficiently.
impl<R: SeekRead, C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting>
    RandomAccessScanIterator<C, D, MultiLayerSpectrum<C, D>> for MzMLReaderType<R, C, D>
{
    fn start_from_id(&mut self, id: &str) -> Result<&Self, ScanAccessError> {
        match self._offset_of_id(id) {
            Some(offset) => match self.seek(SeekFrom::Start(offset)) {
                Ok(_) => Ok(self),
                Err(err) => Err(ScanAccessError::IOError(Some(err))),
            },
            None => Err(ScanAccessError::ScanNotFound),
        }
    }

    fn start_from_index(&mut self, index: usize) -> Result<&Self, ScanAccessError> {
        match self._offset_of_index(index) {
            Some(offset) => match self.seek(SeekFrom::Start(offset)) {
                Ok(_) => Ok(self),
                Err(err) => Err(ScanAccessError::IOError(Some(err))),
            },
            None => Err(ScanAccessError::ScanNotFound),
        }
    }

    fn start_from_time(&mut self, time: f64) -> Result<&Self, ScanAccessError> {
        match self._offset_of_time(time) {
            Some(offset) => match self.seek(SeekFrom::Start(offset)) {
                Ok(_) => Ok(self),
                Err(err) => Err(ScanAccessError::IOError(Some(err))),
            },
            None => Err(ScanAccessError::ScanNotFound),
        }
    }
}

impl<R: SeekRead, C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting> MzMLReaderType<R, C, D> {
    /// Construct a new MzMLReaderType and build an offset index
    /// using [`Self::build_index`]
    pub fn new_indexed(file: R) -> MzMLReaderType<R, C, D> {
        let mut reader = Self::new(file);
        reader.build_index();
        reader
    }

    pub fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.handle.seek(pos)
    }

    /// Builds an offset index to each `<spectrum>` XML element
    /// by doing a fast pre-scan of the XML file.
    pub fn build_index(&mut self) -> u64 {
        let start = self
            .handle
            .stream_position()
            .expect("Failed to save restore location");
        self.seek(SeekFrom::Start(0))
            .expect("Failed to reset stream to beginning");
        let mut reader = Reader::from_reader(&mut self.handle);
        reader.trim_text(true);
        loop {
            match reader.read_event(&mut self.buffer) {
                Ok(Event::Start(ref e)) => {
                    let element_name = e.name();
                    if element_name == b"spectrum" {
                        // Hit a spectrum, extract ID and save current offset

                        for attr_parsed in e.attributes() {
                            match attr_parsed {
                                Ok(attr) => {
                                    match attr.key {
                                        b"id" => {
                                            let scan_id = attr
                                                .unescape_and_decode_value(&reader)
                                                .expect("Error decoding id");
                                            // This count is off by 2 because somehow the < and > bytes are removed?
                                            self.index.insert(
                                                scan_id,
                                                (reader.buffer_position() - e.len() - 2) as u64,
                                            );
                                            break;
                                        }
                                        &_ => {}
                                    };
                                }
                                Err(_msg) => {}
                            }
                        }
                    }
                }
                Ok(Event::End(ref e)) => {
                    let element_name = e.name();
                    if element_name == b"spectrumList" {
                        break;
                    }
                }
                Ok(Event::Eof) => {
                    break;
                }
                _ => {}
            };
            self.buffer.clear();
        }
        let offset = reader.buffer_position() as u64;
        self.handle
            .seek(SeekFrom::Start(start))
            .expect("Failed to restore location");
        self.index.init = true;
        if self.index.len() == 0 {
            warn!("An index was built but no entries were found")
        }
        offset
    }
}

impl<C: CentroidPeakAdapting, D: DeconvolutedPeakAdapting>
    MZFileReader<C, D, MultiLayerSpectrum<C, D>> for MzMLReaderType<fs::File, C, D>
{
    fn open_file(source: fs::File) -> Self {
        Self::new(source)
    }

    fn construct_index_from_stream(&mut self) -> u64 {
        self.build_index()
    }
}

pub type MzMLReader<R> = MzMLReaderType<R, CentroidPeak, DeconvolutedPeak>;

#[cfg(test)]
mod test {
    use super::*;
    use crate::spectrum::spectrum::SpectrumBehavior;
    use std::fs;
    use std::path;

    #[test]
    fn reader_from_file() {
        let path = path::Path::new("./test/data/small.mzML");
        let file = fs::File::open(path).expect("Test file doesn't exist");
        let reader = MzMLReaderType::<_, CentroidPeak, DeconvolutedPeak>::new(file);
        let mut ms1_count = 0;
        let mut msn_count = 0;
        for scan in reader {
            let level = scan.ms_level();
            if level == 1 {
                ms1_count += 1;
            } else {
                msn_count += 1;
            }
        }
        assert_eq!(ms1_count, 14);
        assert_eq!(msn_count, 34);
    }

    #[test]
    fn reader_from_file_indexed() {
        let path = path::Path::new("./test/data/small.mzML");
        let file = fs::File::open(path).expect("Test file doesn't exist");
        let mut reader = MzMLReaderType::<_, CentroidPeak, DeconvolutedPeak>::new_indexed(file);

        let n = reader.len();
        assert_eq!(n, 48);

        let mut ms1_count = 0;
        let mut msn_count = 0;

        for i in (0..n).rev() {
            let scan = reader.get_spectrum_by_index(i).expect("Missing spectrum");
            let level = scan.ms_level();
            if level == 1 {
                ms1_count += 1;
            } else {
                msn_count += 1;
            }
        }
        assert_eq!(ms1_count, 14);
        assert_eq!(msn_count, 34);
    }

    #[test]
    fn reader_from_path() {
        let path = path::Path::new("./test/data/small.mzML");
        let mut reader = MzMLReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(path)
            .expect("Test file doesn't exist?");

        let n = reader.len();
        assert_eq!(n, 48);

        let mut ms1_count = 0;
        let mut msn_count = 0;

        for i in (0..n).rev() {
            let scan = match reader.get_spectrum_by_index(i) {
                Some(scan) => scan,
                None => {
                    if let Some(offset) = reader._offset_of_index(i) {
                        panic!(
                            "Failed to locate spectrum {} at offset {}, parser state {:?}",
                            i, offset, reader.state,
                        );
                    } else {
                        panic!("Failed to locate spectrum or offset {}", i);
                    }
                }
            };
            let level = scan.ms_level();
            if level == 1 {
                ms1_count += 1;
            } else {
                msn_count += 1;
            }
        }
        assert_eq!(ms1_count, 14);
        assert_eq!(msn_count, 34);
    }

    #[test]
    fn grouped_iteration() {
        let path = path::Path::new("./test/data/small.mzML");
        let mut reader = MzMLReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(path)
            .expect("Test file doesn't exist?");

        let n = reader.len();
        assert_eq!(n, 48);

        let mut ms1_count = 0;
        let mut msn_count = 0;

        for group in reader.groups() {
            ms1_count += group.precursor.is_some() as usize;
            msn_count += group.products.len();
        }
        assert_eq!(ms1_count, 14);
        assert_eq!(msn_count, 34);
    }
}
