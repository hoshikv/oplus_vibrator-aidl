mod gen {
    rsbinder::include_aidl!("vibrator");
}
mod gen_oplus {
    rsbinder::include_aidl!("oplus_vibrator");
}
mod effects;
mod log;
mod motor;
mod oplus;
mod service;
mod synth;

use rsbinder::service::{kernel::Host, Registry};

fn main() {
    let service = service::VibratorService::new();
    let inner = service.inner.clone();
    let gen = service.gen.clone();

    let binder: rsbinder::Strong<dyn gen::android::hardware::vibrator::IVibrator::IVibrator> =
        gen::android::hardware::vibrator::IVibrator::BnVibrator::new_binder(service);

    let oplus = oplus::OplusVibrator::new(inner.clone(), gen.clone());
    let oplus_binder =
        gen_oplus::vendor::oplus::hardware::oplusvibrator::IOplusVibrator::BnOplusVibrator::new_binder(
            oplus,
        );
    binder
        .as_binder()
        .set_extension(&oplus_binder.as_binder())
        .unwrap_or_else(|e| eprintln!("failed to set oplus extension: {:?}", e));

    let instance = std::env::var("VIBRATOR_SERVICE_NAME")
        .unwrap_or_else(|_| "android.hardware.vibrator.IVibrator/default".to_string());

    let host = Host::new().unwrap_or_else(|e| {
        eprintln!("failed to init binder host: {:?}", e);
        std::process::exit(1);
    });

    host.add_service(&instance, binder.as_binder())
        .unwrap_or_else(|e| {
            eprintln!("failed to add service {}: {:?}", instance, e);
            std::process::exit(1);
        });

    if instance != "android.hardware.vibrator.IVibrator/default" {
        host.add_service(
            "android.hardware.vibrator.IVibrator/default",
            binder.as_binder(),
        )
        .unwrap_or_else(|e| {
            eprintln!("warn: failed to add /default: {:?}", e);
        });
    }

    let oplus2 = oplus::OplusVibrator::new(inner, gen);
    let oplus_binder2 =
        gen_oplus::vendor::oplus::hardware::oplusvibrator::IOplusVibrator::BnOplusVibrator::new_binder(
            oplus2,
        );
    for name in &[
        "vendor.oplus.hardware.oplusvibrator.IOplusVibrator/default",
        "oplus.hardware.vibrator_oplus_v1",
    ] {
        host.add_service(name, oplus_binder2.as_binder())
            .unwrap_or_else(|e| {
                eprintln!("warn: failed to add oplus service {}: {:?}", name, e);
            });
    }

    eprintln!("vibrator registered: {} + extension + oplus standalone", instance);

    host.serve().expect("serve");
}
