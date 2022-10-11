// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use blst::{
    blst_final_exp, blst_fp12, blst_fp12_mul, blst_fr, blst_miller_loop, blst_p1, blst_p1_affine,
    blst_p1_to_affine, Pairing,
};
use proptest::{collection, prelude::*};
use std::{
    iter,
    ops::{AddAssign, Mul, Neg},
};

use ark_bls12_381::{Bls12_381, Fq12, Fr};
use ark_crypto_primitives::SNARK;
use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff::{One, PrimeField, UniformRand};
use ark_groth16::{Groth16, PreparedVerifyingKey};
use ark_relations::{
    lc,
    r1cs::{ConstraintSynthesizer, ConstraintSystemRef, SynthesisError},
};

use crate::{
    conversions::{
        bls_fq12_to_blst_fp12, bls_fr_to_blst_fr, bls_g1_affine_to_blst_g1_affine,
        bls_g2_affine_to_blst_g2_affine, blst_fp12_to_bls_fq12,
        tests::{arb_bls_fr, arb_bls_g1_affine, arb_blst_g1_affine, arb_blst_g2_affine},
    },
    verifier::{
        g1_linear_combination, multipairing_with_processed_vk, process_vk_special,
        verify_with_processed_vk, Proof, VerifyingKey,
    },
};

// This duplicates the proving DummyCircuit, which is a bit hard to share.
#[derive(Copy, Clone)]
struct DummyCircuit<F: PrimeField> {
    pub a: Option<F>,
    pub b: Option<F>,
    pub num_variables: usize,
    pub num_constraints: usize,
}

impl<F: PrimeField> ConstraintSynthesizer<F> for DummyCircuit<F> {
    // We'll be proving a relationship involving the product c of a & b
    fn generate_constraints(self, cs: ConstraintSystemRef<F>) -> Result<(), SynthesisError> {
        let a = cs.new_witness_variable(|| self.a.ok_or(SynthesisError::AssignmentMissing))?;
        let b = cs.new_witness_variable(|| self.b.ok_or(SynthesisError::AssignmentMissing))?;
        let c = cs.new_input_variable(|| {
            let a = self.a.ok_or(SynthesisError::AssignmentMissing)?;
            let b = self.b.ok_or(SynthesisError::AssignmentMissing)?;

            Ok(a * b)
        })?;

        // a, b, c are above, let's define the rest.
        for _ in 0..(self.num_variables - 3) {
            let _ = cs.new_witness_variable(|| self.a.ok_or(SynthesisError::AssignmentMissing))?;
        }

        for _ in 0..self.num_constraints - 1 {
            cs.enforce_constraint(lc!() + a, lc!() + b, lc!() + c)?;
        }

        cs.enforce_constraint(lc!(), lc!(), lc!())?;

        Ok(())
    }
}

// This emulates the process_vk function of the arkworks verifier, but using blst to compute the term 
// alpha_g1_beta_g2. See [`test_prepare_vk`].
fn ark_process_vk(vk: &VerifyingKey<Bls12_381>) -> PreparedVerifyingKey<Bls12_381> {
    let g1_alpha = bls_g1_affine_to_blst_g1_affine(&vk.alpha_g1);
    let g2_beta = bls_g2_affine_to_blst_g2_affine(&vk.beta_g2);
    let blst_alpha_g1_beta_g2 = {
        let mut tmp = blst_fp12::default();
        unsafe { blst_miller_loop(&mut tmp, &g2_beta, &g1_alpha) };

        let mut out = blst_fp12::default();
        unsafe { blst_final_exp(&mut out, &tmp) };
        out
    };
    let alpha_g1_beta_g2 = blst_fp12_to_bls_fq12(blst_alpha_g1_beta_g2);
    PreparedVerifyingKey {
        vk: vk.clone(),
        alpha_g1_beta_g2,
        gamma_g2_neg_pc: vk.gamma_g2.neg().into(),
        delta_g2_neg_pc: vk.delta_g2.neg().into(),
    }
}

// This commputes the result of the multi-pairing involved in the Groth16 verification, using arkworks.
// See [`test_multipairing_with_processed_vk`] 
pub fn ark_multipairing_with_prepared_vk(
    pvk: &PreparedVerifyingKey<Bls12_381>,
    proof: &Proof<Bls12_381>,
    public_inputs: &[Fr],
) -> Fq12 {
    let mut g_ic = pvk.vk.gamma_abc_g1[0].into_projective();
    for (i, b) in public_inputs.iter().zip(pvk.vk.gamma_abc_g1.iter().skip(1)) {
        g_ic.add_assign(&b.mul(i.into_repr()));
    }

    let qap = Bls12_381::miller_loop(
        [
            (proof.a.into(), proof.b.into()),
            (g_ic.into_affine().into(), pvk.gamma_g2_neg_pc.clone()),
            (proof.c.into(), pvk.delta_g2_neg_pc.clone()),
        ]
        .iter(),
    );

    Bls12_381::final_exponentiation(&qap).unwrap()
}

const LEN: usize = 10;

proptest! {
    // This technical test is necessary because blst does not expose
    // the miller_loop_n operation, and forces us to abuse the signature-oriented pairing engine
    // that it does expose. Here we show the use of the pairing engine is equivalent to iterated
    // use of one-off pairings.
    #[test]
    fn test_blst_miller_loops(
        a_s in collection::vec(arb_blst_g1_affine(), LEN..=LEN),
        b_s in collection::vec(arb_blst_g2_affine(), LEN..=LEN)

    ) {
        let pairing_engine_result = {
            let dst = [0u8; 3];
            let mut pairing_blst = Pairing::new(false, &dst);
            for (b, a) in b_s.iter().zip(a_s.iter()) {
                pairing_blst.raw_aggregate(b, a);
            }
            pairing_blst.as_fp12() // this implies pairing_blst.commit()
        };

        let mut res = blst_fp12::default();
        let mut loop0 = blst_fp12::default();

        for i in 0..LEN {
            unsafe {
                blst_miller_loop(&mut loop0, b_s[i..].as_ptr(), a_s[i..].as_ptr());
                blst_fp12_mul(&mut res, &res, &loop0);
                loop0 = blst_fp12::default();
            }
        }

        prop_assert_eq!(res, pairing_engine_result);
    }

}

#[test]
fn test_prepare_vk() {
    const PUBLIC_SIZE: usize = 128;
    let rng = &mut ark_std::test_rng();
    let c = DummyCircuit::<Fr> {
        a: Some(<Fr>::rand(rng)),
        b: Some(<Fr>::rand(rng)),
        num_variables: PUBLIC_SIZE,
        num_constraints: 65536,
    };

    let (_pk, vk) = Groth16::<Bls12_381>::circuit_specific_setup(c, rng).unwrap();

    let ark_pvk = Groth16::<Bls12_381>::process_vk(&vk).unwrap();
    let blst_pvk = ark_process_vk(&vk);
    assert_eq!(ark_pvk.alpha_g1_beta_g2, blst_pvk.alpha_g1_beta_g2);
}

proptest! {
    #[test]
    fn test_g1_linear_combination(
        frs in collection::vec(arb_bls_fr(), LEN-1..=LEN-1),
        a_s in collection::vec(arb_bls_g1_affine(), LEN..=LEN),
    ) {

        let pts: Vec<blst_p1_affine> = a_s
            .iter()
            .map(bls_g1_affine_to_blst_g1_affine)
            .collect();
        // TODO: find a more direct way to grab this constant
        let one = bls_fr_to_blst_fr(&Fr::one());
        let ss: Vec<blst_fr> = iter::once(one)
            .chain(frs.iter().map(bls_fr_to_blst_fr))
            .collect();
        let mut blst_res = blst_p1::default();
        g1_linear_combination(&mut blst_res, &pts, &ss[..], ss.len());
        let mut blst_res_affine = blst_p1_affine::default();
        unsafe { blst_p1_to_affine(&mut blst_res_affine, &blst_res) };

        let mut g_ic = a_s[0].into_projective();
        for (i, b) in frs.iter().zip(a_s.iter().skip(1)) {
            g_ic.add_assign(&b.mul(i.into_repr()));
        }

        // TODO: convert this so we can make a projective comparison
        prop_assert_eq!(blst_res_affine, bls_g1_affine_to_blst_g1_affine(&g_ic.into_affine()));

    }
}

#[test]
fn test_verify_with_processed_vk() {
    const PUBLIC_SIZE: usize = 128;
    let rng = &mut ark_std::test_rng();
    let c = DummyCircuit::<Fr> {
        a: Some(<Fr>::rand(rng)),
        b: Some(<Fr>::rand(rng)),
        num_variables: PUBLIC_SIZE,
        num_constraints: 65536,
    };

    let (pk, vk) = Groth16::<Bls12_381>::circuit_specific_setup(c, rng).unwrap();
    let proof = Groth16::<Bls12_381>::prove(&pk, c, rng).unwrap();
    let v = c.a.unwrap().mul(c.b.unwrap());

    let blst_pvk = process_vk_special(&vk);

    assert!(verify_with_processed_vk(&blst_pvk, &[v], &proof).unwrap());
}

#[test]
fn test_multipairing_with_processed_vk() {
    const PUBLIC_SIZE: usize = 128;
    let rng = &mut ark_std::test_rng();
    let c = DummyCircuit::<Fr> {
        a: Some(<Fr>::rand(rng)),
        b: Some(<Fr>::rand(rng)),
        num_variables: PUBLIC_SIZE,
        num_constraints: 65536,
    };

    let (pk, vk) = Groth16::<Bls12_381>::circuit_specific_setup(c, rng).unwrap();
    let proof = Groth16::<Bls12_381>::prove(&pk, c, rng).unwrap();
    let v = c.a.unwrap().mul(c.b.unwrap());

    let ark_pvk = Groth16::<Bls12_381>::process_vk(&vk).unwrap();
    let blst_pvk = process_vk_special(&vk);

    let ark_fe = ark_multipairing_with_prepared_vk(&ark_pvk, &proof, &[v]);
    let blst_fe = multipairing_with_processed_vk(&blst_pvk, &[v], &proof);

    assert_eq!(bls_fq12_to_blst_fp12(ark_fe), blst_fe);
}