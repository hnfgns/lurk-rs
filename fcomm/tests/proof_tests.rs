use assert_cmd::prelude::*;
use lurk::state::initial_lurk_state;
use predicates::prelude::*;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use tempfile::{Builder, TempDir};

use pasta_curves::pallas;

use fcomm::{file_map::FileStore, Commitment, CommittedExpression, LurkPtr, Proof};
use lurk::store::Store;

use camino::Utf8Path;

pub type S1 = pallas::Scalar;

fn fcomm_cmd() -> std::process::Command {
    Command::cargo_bin("fcomm").unwrap()
}

#[test]
fn test_bad_command() {
    let mut cmd = fcomm_cmd();

    cmd.arg("uiop");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand \'uiop\'"));
}

#[test]
fn test_eval_expression() {
    let mut cmd = fcomm_cmd();

    let expression = "((lambda (a b) (+ (* a 3) b)) 9 7)";

    let tmp_dir = Builder::new().prefix("tmp").tempdir().unwrap();
    let expression_path = tmp_dir.path().join("expression.lurk");

    let mut expression_file = File::create(&expression_path).unwrap();
    write!(expression_file, "{expression}").unwrap();

    cmd.arg("eval")
        .arg("--expression")
        .arg(expression_path)
        .arg("--lurk");

    cmd.assert()
        .success()
        .stdout("{\"expr\":\"((lambda (.lurk.user.a .lurk.user.b) (+ (* .lurk.user.a 3) .lurk.user.b)) 9 7)\",\"env\":\"nil\",\"cont\":\"Outermost\",\"expr_out\":\"34\",\"env_out\":\"nil\",\"cont_out\":\"Terminal\",\"status\":\"Terminal\",\"iterations\":17}");
}

fn test_prove_expression(
    cmd: &mut Command,
    expression_path: &Utf8Path,
    proof_path: &Utf8Path,
    data_path: &Utf8Path,
) {
    cmd.env("fcomm_data_path", data_path)
        .arg("prove")
        .arg("--expression")
        .arg(expression_path)
        .arg("--proof")
        .arg(proof_path)
        .arg("--lurk");

    cmd.assert().success();
}

fn test_open_commitment(
    mut cmd: Command,
    commitment: String,
    input_path: &Utf8Path,
    proof_path: &Utf8Path,
    data_path: &Utf8Path,
    chained: bool,
) {
    cmd.env("fcomm_data_path", data_path)
        .arg("open")
        .arg("--commitment")
        .arg(commitment)
        .arg("--input")
        .arg(input_path)
        .arg("--proof")
        .arg(proof_path);

    if chained {
        cmd.arg("--chain");
    };

    cmd.assert().success();
}

fn test_verify_expression_proof(mut cmd: Command, proof_path: &Utf8Path, _data_path: &Utf8Path) {
    cmd.arg("verify").arg("--proof").arg(proof_path);

    cmd.assert().success().stdout("{\"verified\":true}");
}

fn test_verify_opening(mut cmd: Command, proof_path: &Utf8Path, _data_path: &Utf8Path) {
    cmd.arg("verify").arg("--proof").arg(proof_path);

    cmd.assert().success().stdout("{\"verified\":true}");
}

#[test]
#[ignore]
fn test_prove_and_verify_expression() {
    let expression = "(* 9 7)";
    let expected = "63";

    let tmp_dir = Builder::new().prefix("tmp").tempdir().unwrap();
    let tmp_dir_path = Utf8Path::from_path(tmp_dir.path()).unwrap();
    let proof_path = tmp_dir_path.join("proof.json");
    let fcomm_data_path = tmp_dir_path.join("fcomm_data");
    let expression_path = tmp_dir_path.join("expression.lurk");

    let mut expression_file = File::create(&expression_path).unwrap();
    write!(expression_file, "{expression}").unwrap();

    {
        test_prove_expression(
            &mut fcomm_cmd(),
            &expression_path,
            &proof_path,
            &fcomm_data_path,
        );

        let proof = Proof::<S1>::read_from_json_path(&proof_path).unwrap();

        assert_eq!(
            proof
                .claim
                .evaluation()
                .expect("expected evaluation claim")
                .expr_out,
            expected
        );
    }

    test_verify_expression_proof(fcomm_cmd(), &proof_path, &fcomm_data_path);
}

fn commit(function_path: &Utf8Path, commitment_path: &Utf8Path, data_path: &Utf8Path) {
    let mut cmd = fcomm_cmd();
    cmd.env("fcomm_data_path", data_path)
        .arg("commit")
        .arg("--function")
        .arg(function_path)
        .arg("--commitment")
        .arg(commitment_path)
        .assert()
        .success();
}

fn test_create_open_and_verify_functional_commitment_aux(
    function_source: &str,
    function_input: &str,
    expected_output: &str,
) {
    let tmp_dir = Builder::new().prefix("tmp").tempdir().unwrap();

    test_aux(
        function_source,
        vec![(function_input, expected_output)],
        false,
        tmp_dir,
    );
}

fn test_create_open_and_verify_chained_functional_commitment_aux(
    function_source: &str,
    expected_io: Vec<(&str, &str)>,
) {
    let tmp_dir = Builder::new().prefix("tmp").tempdir().expect("tmp dir");

    test_aux(function_source, expected_io, true, tmp_dir);
}

fn test_aux(
    function_source: &str,
    expected_io: Vec<(&str, &str)>,
    chained: bool,
    tmp_dir: TempDir,
) {
    let function = CommittedExpression::<S1> {
        expr: LurkPtr::Source(function_source.into()),
        secret: None,
        commitment: None,
    };

    test_function_aux(function, expected_io, chained, tmp_dir)
}

fn test_function_aux(
    function: CommittedExpression<S1>,
    expected_io: Vec<(&str, &str)>,
    chained: bool,
    tmp_dir: TempDir,
) {
    use lurk::writer::Write;

    let io = expected_io.iter();

    let tmp_dir_path = Utf8Path::from_path(tmp_dir.path()).expect("tmp dir path");
    let proof_path = tmp_dir_path.join("proof.json");
    let function_path = tmp_dir_path.join("function.json");
    let input_path = tmp_dir_path.join("input.lurk");
    let commitment_path = tmp_dir_path.join("commitment.json");
    let fcomm_data_path = tmp_dir_path.join("fcomm_data");

    function.write_to_json_path(&function_path);

    commit(&function_path, &commitment_path, &fcomm_data_path);

    let mut commitment: Commitment<S1> =
        Commitment::read_from_json_path(&commitment_path).expect("read commitment");

    for (function_input, expected_output) in io {
        let mut input_file = File::create(&input_path).expect("create file");

        write!(input_file, "{function_input}").expect("write file");

        test_open_commitment(
            fcomm_cmd(),
            commitment.to_string(),
            &input_path,
            &proof_path,
            &fcomm_data_path,
            chained,
        );

        let proof = Proof::<S1>::read_from_json_path(&proof_path).expect("read proof");
        let opening = proof.claim.opening().expect("expected opening claim");

        let mut store = Store::<S1>::default();

        let input = store.read(function_input).expect("store read");
        let canonical_input = input.fmt_to_string(&store, initial_lurk_state());

        let canonical_output = store
            .read(expected_output)
            .expect("store read")
            .fmt_to_string(&store, initial_lurk_state());

        assert_eq!(canonical_input, opening.input);
        assert_eq!(*expected_output, canonical_output);

        test_verify_opening(fcomm_cmd(), &proof_path, &fcomm_data_path);

        if chained {
            match opening.new_commitment {
                Some(c) => commitment = c,
                _ => panic!("new commitment missing"),
            }
        }
    }
}

#[test]
#[ignore]
fn test_create_open_and_verify_functional_commitment() {
    let function_source = "(lambda (x) (+ x 3))";
    let function_input = "22";
    let expected_output = "25";
    test_create_open_and_verify_functional_commitment_aux(
        function_source,
        function_input,
        expected_output,
    );
}

#[test]
#[ignore]
fn test_create_open_and_verify_higher_order_functional_commitment() {
    let function_source = "(lambda (f) (+ (f 3) 1))";
    let function_input = "(lambda (x) (* x 5))";
    let expected_output = "16";
    test_create_open_and_verify_functional_commitment_aux(
        function_source,
        function_input,
        expected_output,
    );
}

#[test]
#[ignore]
fn test_create_open_and_verify_chained_functional_commitment() {
    let function_source = "(letrec ((secret 12345) (a (lambda (acc x) (let ((acc (+ acc x))) (cons acc (hide secret (a acc))))))) (a 0))";

    let expected_io = vec![("5", "5"), ("3", "8")];

    test_create_open_and_verify_chained_functional_commitment_aux(function_source, expected_io);
}

#[test]
#[ignore]
fn test_create_open_and_verify_complicated_higher_order_functional_commitment1() {
    let function_source = "(let ((nums '(1 2 3 4 5))) (lambda (f) (f nums)))";
    let function_input = "(letrec ((sum-aux (lambda (acc nums)
                                              (if nums
                                                (sum-aux (+ acc (car nums)) (cdr nums))
                                                acc)))
                                   (sum (sum-aux 0)))
                             (lambda (nums)
                               (sum nums)))";
    let expected_output = "15";

    test_create_open_and_verify_functional_commitment_aux(
        function_source,
        function_input,
        expected_output,
    );
}

#[test]
#[ignore]
fn test_create_open_and_verify_complicated_higher_order_functional_commitment2() {
    let function_source = "(letrec ((secret-data '((joe 4 3) (bill 10 2 3) (jane 8 7 6 10) (carol 3 5 8))) (filter (lambda (data predicate) (if data (if (predicate (cdr (car data))) (cons (car data) (filter (cdr data) predicate)) (filter (cdr data) predicate))))) (f (lambda (predicate) (car (car (filter secret-data predicate)))))) f)";

    let function_input = "(letrec ((sum-aux (lambda (acc nums)
                                              (if nums
                                                (sum-aux (+ acc (car nums)) (cdr nums))
                                                acc)))
                                   (sum (sum-aux 0)))
                             (lambda (nums)
                               (= (sum nums) 15)))";
    let expected_output = ".lurk.user.BILL";

    test_create_open_and_verify_functional_commitment_aux(
        function_source,
        function_input,
        expected_output,
    );
}
