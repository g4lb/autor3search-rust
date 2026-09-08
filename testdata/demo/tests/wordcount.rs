use demo::count_words;

#[test]
fn strips_punctuation_and_folds_case() {
    let got = count_words("Hello, WORLD! hello?");
    assert_eq!(got["hello"], 2);
    assert_eq!(got["world"], 1);
    assert_eq!(got.len(), 2);
}

#[test]
fn digits_are_kept_and_empty_input_yields_nothing() {
    let got = count_words("abc 123 !!!");
    assert_eq!(got["abc"], 1);
    assert_eq!(got["123"], 1);
    assert_eq!(got.len(), 2);
    assert!(count_words("").is_empty());
}
