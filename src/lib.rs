//! This zero-delay feedback filter is based on a state variable filter.
//! It follows the following equations:
//!
//! Since we can't easily solve a nonlinear equation,
//! Mystran's fixed-pivot method is used to approximate the tanh() parts.
//! Quality can be improved a lot by oversampling a bit.
//! Damping feedback is antisaturated, so it doesn't disappear at high gains.

// TODO:
// look into successive over-relaxation, Gauss–Seidel method, just making a runge-kutta solver
// Brent's method seems the most promising so far. Could potentially replace inverse quadratic with newton's
// or possibly just a broyden method fallback, can't be bothered working much more on this lol: http://fabcol.free.fr/pdf/lectnotes5.pdf
// check if it's well-behaved without the pivotal guess, and how to make pivotal more similar to newton?

#[macro_use]
extern crate vst;
use std::f32::consts::PI;
use std::sync::Arc;

use vst::buffer::AudioBuffer;
use vst::editor::Editor;
use vst::plugin::{Category, HostCallback, Info, Plugin, PluginParameters};

mod editor;
use editor::{EditorState, SVFPluginEditor};
mod parameter;
#[allow(dead_code)]
mod utils;
use utils::AtomicOps;
mod filter_parameters;
use filter_parameters::FilterParameters;
enum _Mode {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
    Peak,
}
#[allow(dead_code)]
#[derive(PartialEq, Clone, Copy)]
enum EstimateSource {
    State,               // use current state
    PreviousVout,        // use z-1 of Vout
    LinearStateEstimate, // use linear estimate of future state
    LinearVoutEstimate,  // use linear estimate of Vout
}

// this is a 2-pole filter with resonance, which is why there's 2 states and vouts
struct SVF {
    // Store a handle to the plugin's parameter object.
    params: Arc<FilterParameters>,
    // The object responsible for the gui
    editor: Option<SVFPluginEditor>,
    // the output of the different filter stages
    vout: [f32; 2],
    // s is the "state" parameter. In an IIR it would be the last value from the filter
    // In this we find it by trapezoidal integration to avoid the unit delay
    s: [f32; 2],
}

// member methods for the struct
#[allow(dead_code)]
impl SVF {
    // the state needs to be updated after each process. Found by trapezoidal integration
    fn update_state(&mut self) {
        self.s[0] = 2. * self.vout[0] - self.s[0];
        self.s[1] = 2. * self.vout[1] - self.s[1];
    }
    fn get_estimate(&mut self, n: usize, estimate: EstimateSource, input: f32) -> f32 {
        // if we ask for an estimate based on the linear filter, we have to run it
        if estimate == EstimateSource::LinearStateEstimate
            || estimate == EstimateSource::LinearVoutEstimate
        {
            self.run_svf_linear(input);
        }
        match estimate {
            EstimateSource::State => self.s[n],
            EstimateSource::PreviousVout => self.vout[n],
            EstimateSource::LinearStateEstimate => 2. * self.vout[n] - self.s[n],
            EstimateSource::LinearVoutEstimate => self.vout[n],
        }
    }

    // performs a complete filter process (fixed-pivot method)
    fn tick_pivotal(&mut self, input: f32) -> f32 {
        // perform filter process
        let out = self.run_svf_pivotal(input * (self.params.drive.get() + 1.));
        // update ic1eq and ic2eq for next sample
        self.update_state();
        out
    }
    // performs a complete filter process (fixed-pivot method)
    fn tick_newton(&mut self, input: f32) -> f32 {
        // perform filter process
        let out = self.run_svf_newton(input * (self.params.drive.get() + 1.));
        // update ic1eq and ic2eq for next sample
        self.update_state();
        out
    }
    pub fn run_svf_linear(&mut self, input: f32) -> f32 {
        let g = self.params.g.get();
        // declaring some constants that simplifies the math a bit
        let k = self.params.res.get();
        let g1 = 1. / (1. + g * (g + k));
        let g2 = g * g1;
        // let g3 = g * g2;
        // outputs the correct output voltages
        self.vout[0] = g1 * self.s[0] + g2 * (input - self.s[1]);
        // self.vout[1] = (input - self.s[1]) * g3 + self.s[0] * g2 + self.s[1]; <- meant for parallel processing
        self.vout[1] = self.s[1] + g * self.vout[0];
        match self.params.mode.get() {
            0 => self.vout[1],                            // lowpass
            1 => input - k * self.vout[0] - self.vout[1], // highpass
            2 => self.vout[0],                            // bandpass
            3 => input - k * self.vout[0],                // notch
            //3 => input - 2. * k * self.vout[1], // allpass
            4 => input - 2. * self.vout[1] - k * self.vout[0], // peak
            _ => k * self.vout[0],                             // bandpass (normalized peak gain)
        }
    }
    pub fn run_svf_pivotal(&mut self, input: f32) -> f32 {
        // ---------- setup ----------
        // load in g and k from parameters
        let g = self.params.g.get();
        let k = self.params.res.get();
        // a[n] is the fixed-pivot approximation for whatever is being processed nonlinearly
        let mut a = [1.; 3];
        let est_type = EstimateSource::State;
        // first getting fixed-pivot approximation for the feedback line, since it's necessary for computing a[0]:
        let est_source_a2 = self.get_estimate(0, est_type, input);
        // employing fixed-pivot method
        if est_source_a2 != 0. {
            // v_t and i_s are constants to control the diode clipper's character
            // just earballed em to be honest. Hard to figure out what they should be
            // without knowing the circuit's operating voltage and temperature
            let v_t = 4.;
            let i_s = 4.;
            // a2 is clipped with the inverse of the diode anti-saturator
            a[2] = (v_t * (est_source_a2 / i_s).asinh()) / est_source_a2;
        }
        let est_source_rest = [
            (input
                - (est_source_a2 * a[2] + (k - 1.) * est_source_a2)
                - self.get_estimate(1, est_type, input)),
            self.get_estimate(0, est_type, input),
        ];
        for n in 0..est_source_rest.len() {
            if est_source_rest[n] != 0. {
                a[n] = est_source_rest[n].tanh() / est_source_rest[n];
            } else {
            }
        }
        // ---------- calculations ----------
        // factored out of the equation
        let g1 = 1. / (g * a[0]);
        let g2 = 1. / (a[0] * a[2] * g * g1 * k - a[0] * a[2] * g * g1 + a[2] * g1 + 1.);
        let g3 = 1. / (1. + g.powi(2) * a[0] * a[1] * g1 * g2 * a[2]);
        // solving equations for output voltages at v1 and v2
        let u = (g * a[0] * input - g * a[0] * self.s[1] + self.s[0]) * g1 * g2 * g3;
        self.vout[0] = u.asinh();
        self.vout[1] = g * a[1] * self.vout[0] + self.s[1];
        // here, the output is chosen to give the specified type of filter
        match self.params.mode.get() {
            0 => self.vout[1],                            // lowpass
            1 => input - k * self.vout[0] - self.vout[1], // highpass
            2 => self.vout[0],                            // bandpass
            3 => input - k * self.vout[0],                // notch
            //3 => input - 2. * k * self.vout[1], // allpass
            4 => input - 2. * self.vout[1] - k * self.vout[0], // peak
            _ => k * self.vout[0],                             // bandpass (normalized peak gain)
        }
    }
    // trying to avoid having to invert the matrix
    pub fn run_svf_newton(&mut self, input: f32) -> f32 {
        // ---------- setup ----------
        // load in g and k from parameters
        let g = self.params.g.get();
        let k = self.params.res.get();
        // a[n] is the fixed-pivot approximation for whatever is being processed nonlinearly
        let mut v_est: [f32; 2];
        let est_type = EstimateSource::LinearVoutEstimate;
        // let est_type = EstimateSource::State;

        // getting initial estimate. Could potentially be done with the fixed_pivot filter
        v_est = [
            self.get_estimate(0, est_type, input),
            self.get_estimate(1, est_type, input),
        ];
        let mut sinh_v_est0 = v_est[0].sinh();
        let mut tanh_v_est0 = v_est[0].tanh();
        let mut fb_line = (input - ((k - 1.) * v_est[0] + sinh_v_est0) - v_est[1]).tanh();
        // using fixed_pivot as estimate
        // self.run_svf_pivotal(input);
        // v_est = [self.vout[0], self.vout[1]];
        let mut filter_out = self.run_svf(g, tanh_v_est0, fb_line);
        let mut residue = [filter_out[0] - v_est[0], filter_out[1] - v_est[1]];

        // println!("residue: {:?}", residue);
        let max_error = 0.00001;
        let mut n_iterations = 0;
        while residue[0].abs() > max_error || residue[1].abs() > max_error {
            if n_iterations > 10 {
                // panic!("infinite loop mayhaps?");
                // println!("infinite loop mayhaps?");
                break;
            }
            // TODO: not sure why this can't start out as uninitialized
            let mut jacobian: [[f32; 2]; 2] = [[-1.; 2]; 2];
            // factored out of the derivatives
            // let bigboy = (v_est[0] * k + sinh_v_est0 - input - v_est[0] + v_est[1]).cosh().powi(2);
            let bigboy = 1. / (1. - fb_line * fb_line);
            // since the thing that happens at j[0][0] is that it goes towards -1 at low values
            // (everything else than bigboy becomes really small), if it ever is NaN (overflow), we just set it to -1
            if bigboy.is_infinite() {
                // println!("bigboy is inf");
                jacobian[0][0] = -1.;
                jacobian[0][1] = 0.;
            } else {
                // jacobian[0][0] = (-bigboy - (g * (k - 1. + (v_est[0]).cosh()))) / bigboy;
                // Note: If you replace sinh or tanh with an approximation, sinh_v_est0/tanh_v_est0 needs to be change to dy/dx (sinh(x))
                jacobian[0][0] = (-bigboy - (g * (k - 1. + sinh_v_est0 / tanh_v_est0))) / bigboy;
                jacobian[0][1] = -(g / bigboy);
            }
            // jacobian[1][0] = g * (v_est[0].cosh().powi(2));
            jacobian[1][0] = g * (1. - tanh_v_est0 * tanh_v_est0);
            // jacobian[1][1] = -1.;

            v_est[0] = (jacobian[0][1] * jacobian[1][0] * v_est[0] + jacobian[0][0] * v_est[0]
                - jacobian[0][1] * residue[1]
                - residue[0])
                / (jacobian[0][1] * jacobian[1][0] + jacobian[0][0]);
            v_est[1] = (jacobian[0][1] * jacobian[1][0] * v_est[1]
                + jacobian[0][0] * residue[1]
                + jacobian[0][0] * v_est[1]
                - jacobian[1][0] * residue[0])
                / (jacobian[0][1] * jacobian[1][0] + jacobian[0][0]);
            sinh_v_est0 = v_est[0].sinh();
            tanh_v_est0 = v_est[0].tanh();
            fb_line = (input - ((k - 1.) * v_est[0] + sinh_v_est0) - v_est[1]).tanh();
            // recompute filter
            filter_out = self.run_svf(g, tanh_v_est0, fb_line);
            residue = [filter_out[0] - v_est[0], filter_out[1] - v_est[1]];
            // println!("estimate: {:?}", v_est);
            // println!("residue: {:?}", residue);
            n_iterations += 1;
        }
        // when newton's method is done, we have some good estimates for vout
        // println!("---- success ----");
        // println!("n_iterations: {}", n_iterations);

        self.vout[0] = v_est[0];
        self.vout[1] = v_est[1];

        // here, the output is chosen to give the specified type of filter
        match self.params.mode.get() {
            0 => self.vout[1],                            // lowpass
            1 => input - k * self.vout[0] - self.vout[1], // highpass
            2 => self.vout[0],                            // bandpass
            3 => input - k * self.vout[0],                // notch
            //3 => input - 2. * k * self.vout[1], // allpass
            4 => input - 2. * self.vout[1] - k * self.vout[0], // peak
            _ => k * self.vout[0],                             // bandpass (normalized peak gain)
        }
    }
    // TODO: This should probably use the (is * (v_est_0 / vt).sinh() formula for slightly nicer resonance levels
    // TODO: Has some weird temporary self-oscillation at some settings. Maybe change to 2 or smth?
    // That might still keep it consistent at converging (if oversampled 2x)
    pub fn run_svf_newton_less_antisat(&mut self, input: f32) -> f32 {
        // ---------- setup ----------
        // load in g and k from parameters
        let g = self.params.g.get();
        let k = self.params.res.get();
        // a[n] is the fixed-pivot approximation for whatever is being processed nonlinearly
        let mut v_est: [f32; 2];
        let est_type = EstimateSource::LinearVoutEstimate;
        // let est_type = EstimateSource::State;

        // getting initial estimate. Could potentially be done with the fixed_pivot filter
        v_est = [
            self.get_estimate(0, est_type, input),
            self.get_estimate(1, est_type, input),
        ];
        let mut sinh_v_est0 = 1.5 * (v_est[0] / 1.5).sinh();
        let mut tanh_v_est0 = v_est[0].tanh();
        let mut fb_line = (input - ((k - 1.) * v_est[0] + sinh_v_est0) - v_est[1]).tanh();
        // using fixed_pivot as estimate
        self.run_svf_pivotal(input);
        v_est = [self.vout[0], self.vout[1]];
        let mut filter_out = self.run_svf(g, tanh_v_est0, fb_line);
        let mut residue = [filter_out[0] - v_est[0], filter_out[1] - v_est[1]];

        let max_error = 0.00001;
        let mut n_iterations = 0;
        while residue[0].abs() > max_error || residue[1].abs() > max_error {
            if n_iterations > 10 {
                // panic!("infinite loop mayhaps?");
                break;
            }
            // TODO: not sure why this can't start out as uninitialized
            let mut jacobian: [[f32; 2]; 2] = [[-1.; 2]; 2];
            // factored out of the derivatives
            let bigboy = 1. / (1. - fb_line * fb_line);

            // since the thing that happens at j[0][0] is that it goes towards -1 at low values
            // (everything else than bigboy becomes really small), if it ever is NaN (overflow), we just set it to -1
            if bigboy.is_infinite() {
                // println!("bigboy is inf");
                jacobian[0][0] = -1.;
                jacobian[0][1] = 0.;
            } else {
                // jacobian[0][0] = (-bigboy - (g * (k - 1. + (v_est[0]).cosh()))) / bigboy;
                jacobian[0][0] = (-bigboy - (g * (k - 1. + (v_est[0] / 1.5).cosh()))) / bigboy;
                jacobian[0][1] = -(g / bigboy);
            }
            jacobian[1][0] = g * (1. - tanh_v_est0.powi(2));
            // jacobian[1][1] = -1.;

            v_est[0] = (jacobian[0][1] * jacobian[1][0] * v_est[0] + jacobian[0][0] * v_est[0]
                - jacobian[0][1] * residue[1]
                - residue[0])
                / (jacobian[0][1] * jacobian[1][0] + jacobian[0][0]);
            v_est[1] = (jacobian[0][1] * jacobian[1][0] * v_est[1]
                + jacobian[0][0] * residue[1]
                + jacobian[0][0] * v_est[1]
                - jacobian[1][0] * residue[0])
                / (jacobian[0][1] * jacobian[1][0] + jacobian[0][0]);

            sinh_v_est0 = 2. * (v_est[0] / 2.).sinh();
            tanh_v_est0 = v_est[0].tanh();
            fb_line = (input - ((k - 1.) * v_est[0] + sinh_v_est0) - v_est[1]).tanh();
            // recompute filter
            filter_out = self.run_svf(g, tanh_v_est0, fb_line);
            residue = [filter_out[0] - v_est[0], filter_out[1] - v_est[1]];
            n_iterations += 1;
        }
        // when newton's method is done, we have some good estimates for vout
        // println!("---- success ----");
        // println!("n_iterations: {}", n_iterations);

        self.vout[0] = v_est[0];
        self.vout[1] = v_est[1];

        // here, the output is chosen to give the specified type of filter
        match self.params.mode.get() {
            0 => self.vout[1],                            // lowpass
            1 => input - k * self.vout[0] - self.vout[1], // highpass
            2 => self.vout[0],                            // bandpass
            3 => input - k * self.vout[0],                // notch
            //3 => input - 2. * k * self.vout[1], // allpass
            4 => input - 2. * self.vout[1] - k * self.vout[0], // peak
            _ => k * self.vout[0],                             // bandpass (normalized peak gain)
        }
    }
    /// helper function for newton's method
    #[inline]
    pub fn run_svf(&mut self, g: f32, tanh_v_est0: f32, fb_line: f32) -> [f32; 2] {
        let mut out: [f32; 2] = [1.; 2];
        out[0] = g * fb_line + self.s[0];
        out[1] = g * tanh_v_est0 + self.s[1];

        out
    }
}
impl FilterParameters {
    pub fn update_g(&self) {
        self.g
            .set((PI * self.cutoff.get() / (self.sample_rate.get())).tan());
    }
}
impl PluginParameters for FilterParameters {
    fn get_parameter(&self, index: i32) -> f32 {
        match index {
            0 => self.cutoff.get_normalized(),
            1 => self.res.get_normalized(),
            2 => self.drive.get_normalized(),
            3 => self.mode.get_normalized() as f32,
            _ => 0.0,
        }
    }
    fn set_parameter(&self, index: i32, value: f32) {
        match index {
            0 => {
                self.cutoff.set_normalized(value);
                self.update_g();
            }
            1 => self.res.set_normalized(value),
            2 => self.drive.set_normalized(value),
            3 => self.mode.set_normalized(value),
            _ => (),
        }
    }
    fn get_parameter_name(&self, index: i32) -> String {
        match index {
            0 => "cutoff".to_string(),
            1 => "resonance".to_string(),
            2 => "drive".to_string(),
            3 => "filter mode".to_string(),
            4 => "dry/wet".to_string(),
            _ => "".to_string(),
        }
    }
    fn get_parameter_label(&self, index: i32) -> String {
        match index {
            // 0 => "Hz".to_string(),
            // 1 => "%".to_string(),
            // 2 => "".to_string(),
            // 4 => "%".to_string(),
            _ => "".to_string(),
        }
    }
    // This is what will display underneath our control.  We can
    // format it into a string that makes sense for the user.
    fn get_parameter_text(&self, index: i32) -> String {
        match index {
            0 => self.cutoff.get_display(),
            1 => self.res.get_display(),
            // 2 => format!("{:.2}", 20. * (self.drive.get() + 1.).log10()),
            2 => self.drive.get_display(),
            3 => self.mode.get_display(),
            _ => format!(""),
        }
    }
}
impl Default for SVF {
    fn default() -> Self {
        let params = Arc::new(FilterParameters::default());
        Self {
            vout: [0f32; 2],
            s: [0f32; 2],
            params: params.clone(),
            editor: Some(SVFPluginEditor {
                is_open: false,
                state: Arc::new(EditorState {
                    params: params,
                    host: None,
                }),
            }),
        }
    }
}
impl Plugin for SVF {
    fn new(host: HostCallback) -> Self {
        let params = Arc::new(FilterParameters::default());
        Self {
            vout: [0f32; 2],
            s: [0f32; 2],
            params: params.clone(),
            editor: Some(SVFPluginEditor {
                is_open: false,
                state: Arc::new(EditorState {
                    params,
                    host: Some(host),
                }),
            }),
        }
    }
    fn set_sample_rate(&mut self, rate: f32) {
        self.params.sample_rate.set(rate);
        self.params.update_g();
    }
    fn get_info(&self) -> Info {
        Info {
            name: "SVF".to_string(),
            unique_id: 80371372,
            inputs: 1,
            outputs: 1,
            category: Category::Effect,
            parameters: 4,
            ..Default::default()
        }
    }
    // the DAW calls process every time a buffer of samples needs to be sent through the vst
    // buffer consists of both input and output buffers
    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        // split the buffer into input and output
        for (input_buffer, output_buffer) in buffer.zip() {
            // iterate through each sample in the input and output buffer
            for (input_sample, output_sample) in input_buffer.iter().zip(output_buffer) {
                // get the output sample by processing the input sample
                // *output_sample = self.tick_pivotal(*input_sample);
                *output_sample = self.tick_newton(*input_sample);
            }
        }
    }
    fn get_editor(&mut self) -> Option<Box<dyn Editor>> {
        if let Some(editor) = self.editor.take() {
            Some(Box::new(editor) as Box<dyn Editor>)
        } else {
            None
        }
    }
    // lets the plugin host get access to the parameters
    fn get_parameter_object(&mut self) -> Arc<dyn PluginParameters> {
        Arc::clone(&self.params) as Arc<dyn PluginParameters>
    }
}
plugin_main!(SVF);

#[test]
fn save_filter_impulse() {
    let mut plugin = SVF::default();

    // setting up hound for creating .wav files
    use hound;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44100,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(format!("testing/newton_impulse.wav"), spec).unwrap();
    let len = 100;
    let mut input_sample = 0.5;
    // saving samples to wav file
    for _i in 0..len {
        // let output_sample = plugin.tick_pivotal(input_sample);
        let output_sample = plugin.tick_newton(input_sample);
        println!("out: {}", plugin.vout[0]);
        writer
            // .write_sample(plugin.tick_newton(input_sample))
            .write_sample(output_sample)
            .unwrap();

        input_sample = 0.0;
    }
}
#[test]
fn newton_test() {
    let mut plugin = SVF::default();

    println!("g: {}", plugin.params.g.get());
    let len = 1;
    let mut input_sample = 0.;
    // saving samples to wav file
    for _i in 0..len {
        plugin.tick_newton(input_sample);

        input_sample = 0.;
    }
}
#[test]
fn newton_test_sine() {
    let mut plugin = SVF::default();

    // println!("g: {}", plugin.params.g.get());
    plugin.params.set_parameter(0, 1.);
    plugin.params.set_parameter(1, 1.);
    // println!("g: {}", plugin.params.g.get());
    let len = 1000;
    let amplitude = 25.;
    // saving samples to wav file
    for t in (0..len).map(|x| x as f32 / 48000.) {
        let _sample = plugin.tick_newton(amplitude * (t * 440.0 * 2.0 * PI).sin());
        // let amplitude = i16::MAX as f32;
        // writer.write_sample((sample * amplitude) as i16).unwrap();
    }
    // for _i in 0..len {
    //     plugin.tick_newton(input_sample);

    //     input_sample = 0.;
    // }
}
#[test]
fn newton_test_noise() {
    use rand::Rng;
    let mut plugin = SVF::default();
    let mut rng = rand::thread_rng();
    plugin.params.sample_rate.set(48000.);
    // println!("g: {}", plugin.params.g.get());
    plugin.params.set_parameter(0, 1.);
    plugin.params.set_parameter(1, 1.);
    // println!("g: {}", plugin.params.g.get());
    let len = 1000;
    let amplitude = 25.;
    // saving samples to wav file
    for _t in (0..len).map(|x| x as f32 / 48000.) {
        let _sample = plugin.tick_newton(rng.gen_range(-amplitude..amplitude));
        // let amplitude = i16::MAX as f32;
        // writer.write_sample((sample * amplitude) as i16).unwrap();
    }
    // for _i in 0..len {
    //     plugin.tick_newton(input_sample);

    //     input_sample = 0.;
    // }
}
#[test]
fn matrix_test() {
    let mut jacobian_inv: [[f32; 2]; 2] = [[1.; 2]; 2];

    // there's a 0 in row 1 column 0 that makes it pretty easy to find the inverse jacobian right away
    // TODO: simplify this
    jacobian_inv[0][0] = 1.;
    // jacobian_inv[0][0] = 1./ ((k - 1. + v_est[0].cosh()) * ((v_est[0] * k + v_est[0].sinh() - input - v_est[0] + v_est[1]).tanh().powi(2) - 1. ) * g);
    jacobian_inv[0][1] = 2.;
    jacobian_inv[1][0] = 3.;
    jacobian_inv[1][1] = 4.;

    println!("matrix: {:?}", jacobian_inv);

    println!("{:?}", jacobian_inv[0][0]);
    println!("{:?}", jacobian_inv[0][1]);
    println!("{:?}", jacobian_inv[1][0]);
    println!("{:?}", jacobian_inv[1][1]);

    let residue = [1., 2.];

    let minusboy = [
        jacobian_inv[0][0] * residue[0] + jacobian_inv[0][1] * residue[1],
        jacobian_inv[1][0] * residue[0] + jacobian_inv[1][1] * residue[1],
    ];
    println!("minusboy: {:?}", minusboy);
}
#[test]
fn dumbtest() {
    let a: f32 = 1. / 0.;
    println!("{}", -a / a);
    println!("{}", a);
}
